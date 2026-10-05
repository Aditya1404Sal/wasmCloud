package runtime_operator

// This will be moved to go.wasmcloud.dev/runtime-operator/operator.go

import (
	"context"
	"errors"
	"strconv"
	"time"

	"github.com/nats-io/nats.go"
	runtime_controllers "go.wasmcloud.dev/runtime-operator/v2/internal/controller/runtime"
	"go.wasmcloud.dev/runtime-operator/v2/pkg/wasmbus"
	corev1 "k8s.io/api/core/v1"
	"sigs.k8s.io/controller-runtime/pkg/manager"
)

// DefaultHeartbeatTTL is how long a host may go unheard-from before it is
// considered unreachable: four missed heartbeats at the host's 15s interval.
const DefaultHeartbeatTTL = 60 * time.Second

// MinHeartbeatTTL is the shortest TTL the operator honours. Hosts publish every
// 15s, so under two intervals a live fleet reads as unheard between ticks and
// no host is ever reaped.
const MinHeartbeatTTL = 30 * time.Second

type EmbeddedOperatorConfig struct {
	// NATS connection string. Used to communicate with hosts.
	NatsURL string
	// PrecompileNatsURL is the NATS URL the precompile Worker Job connects
	// to (its NATS_URL env var) to write precompiled .cwasm bytes. This is
	// the Data plane endpoint that hosts fetch precompiled components from
	// (wash host --data-nats-url), which may differ from NatsURL (the
	// Scheduler/control-plane endpoint used for host RPC). Empty reuses
	// NatsURL.
	PrecompileNatsURL string
	// NATS options. Used to configure the NATS connection.
	NatsOptions []nats.Option
	// Heartbeat TTL. Used to determine how long to wait before considering a
	// host unreachable. Unset means DefaultHeartbeatTTL; anything under
	// MinHeartbeatTTL is raised to it.
	HeartbeatTTL time.Duration
	// Host CPU threshold (percentage).
	// Used to calculate workload scheduling, avoiding hosts that are over this threshold.
	HostCPUThreshold float64
	// Host Memory threshold (percentage).
	// Used to calculate workload scheduling, avoiding hosts that are over this threshold.
	HostMemoryThreshold float64
	// Disable Artifact Controller. If set, Artifacts must be marked as 'Ready' elsewhere.
	// Useful when introducing a custom artifact management solution.
	DisableArtifactController bool
	// Disable Precompile Controller. If set, no precompile Jobs are emitted.
	DisablePrecompileController bool
	// Container image for the precompile Worker Job.
	PrecompileWorkerImage string
	// Scheme-qualified URL prefix where precompiled .cwasm bytes are written.
	PrecompileArtifactBaseURL string
	// Target triple precompiled bytes are produced for.
	PrecompileTarget string
	// Wasmtime version the worker image links against.
	PrecompileWasmtimeVersion string
	// Comma-separated registries the precompile Worker may pull from over
	// plain HTTP. Mirrors the host's INSECURE_REGISTRIES allowlist.
	PrecompileInsecureRegistries string
	// PrecompileGCInterval is the GC sweep cadence.
	PrecompileGCInterval time.Duration
	// PrecompileGCGracePeriod is the minimum age (by object ModTime) an
	// unreachable object must reach before GC may collect it, guarding the
	// window between a precompile Job writing an object and the operator
	// recording it in Artifact status.
	PrecompileGCGracePeriod time.Duration
	// PrecompileArtifactReplicas is the JetStream replica count for the
	// precompiled-artifacts object store bucket. Only takes effect when the
	// bucket doesn't already exist — the precompile Worker attaches to
	// (rather than reconfigures) an existing bucket's replica count.
	PrecompileArtifactReplicas uint
	// Namespace is the namespace the operator itself runs in. Every Host
	// CRD is created here regardless of where the underlying host pod
	// runs; tenant attribution is carried on the Host's Environment
	// field.
	Namespace string
	// HostNamespaces is the list of namespaces where host Pods run. The
	// operator's Pod informer cache and per-namespace Pod RBAC cover this
	// set so HostPodReconciler can manage finalizers on host Pods.
	HostNamespaces []string
	// AllowSharedHosts controls whether a WorkloadDeployment may schedule
	// onto a Host whose Environment differs from the workload's own
	// namespace (via WorkloadSpec.Environment). Default (true) preserves
	// legacy Cluster-scope scheduling semantics: with Environment unset
	// the scheduler imposes no Environment filter. When false, scheduling
	// is locked to the workload's own namespace and any non-matching
	// Environment is rejected with a Warning Event.
	AllowSharedHosts bool
	// WorkloadConcurrency bounds how many Workloads, WorkloadDeployments,
	// and Hosts are reconciled at once. Defaults to 1 when zero.
	WorkloadConcurrency int
}

// EmbeddedOperator is the main struct for the embedded operator.
// It allows embedding the Runtime Operator into other applications.
type EmbeddedOperator struct {
	Bus      wasmbus.Bus
	NatsConn *nats.Conn
}

// NewEmbeddedOperator creates a new EmbeddedOperator.
func NewEmbeddedOperator(
	ctx context.Context,
	mgr manager.Manager,
	cfg EmbeddedOperatorConfig,
) (*EmbeddedOperator, error) {
	// Validate before any side effects (NATS connect, controller setup).
	// Every Host CRD is created in cfg.Namespace, and the operator's
	// namespaced Role for Host CRUD binds there.
	if cfg.Namespace == "" {
		return nil, errors.New("EmbeddedOperatorConfig.Namespace is required")
	}
	// A replica count of 0 would ask the precompile Worker to create the
	// precompiled-artifacts bucket with zero copies of the data.
	if !cfg.DisablePrecompileController && cfg.PrecompileArtifactReplicas == 0 {
		return nil, errors.New("EmbeddedOperatorConfig.PrecompileArtifactReplicas must be at least 1")
	}

	switch {
	case cfg.HeartbeatTTL <= 0:
		cfg.HeartbeatTTL = DefaultHeartbeatTTL
	case cfg.HeartbeatTTL < MinHeartbeatTTL:
		mgr.GetLogger().Info("HeartbeatTTL raised to the minimum the host heartbeat interval allows",
			"configured", cfg.HeartbeatTTL, "using", MinHeartbeatTTL)
		cfg.HeartbeatTTL = MinHeartbeatTTL
	}

	switch {
	case cfg.HeartbeatTTL <= 0:
		cfg.HeartbeatTTL = DefaultHeartbeatTTL
	case cfg.HeartbeatTTL < MinHeartbeatTTL:
		mgr.GetLogger().Info("HeartbeatTTL raised to the minimum the host heartbeat interval allows",
			"configured", cfg.HeartbeatTTL, "using", MinHeartbeatTTL)
		cfg.HeartbeatTTL = MinHeartbeatTTL
	}

	nc, err := wasmbus.NatsConnectContext(ctx, cfg.NatsURL, cfg.NatsOptions...)
	if err != nil {
		return nil, err
	}
	bus := wasmbus.NewNatsBus(nc)
	retiredHosts := &runtime_controllers.RetiredHosts{}

	if !cfg.DisableArtifactController {
		if err = (&runtime_controllers.ArtifactReconciler{
			Client: mgr.GetClient(),
			Scheme: mgr.GetScheme(),
		}).SetupWithManager(mgr); err != nil {
			return nil, err
		}
	}

	if !cfg.DisablePrecompileController {
		precompileNatsURL := cfg.PrecompileNatsURL
		if precompileNatsURL == "" {
			precompileNatsURL = cfg.NatsURL
		}
		if err = (&runtime_controllers.PrecompileReconciler{
			Client:      mgr.GetClient(),
			Scheme:      mgr.GetScheme(),
			WorkerImage: cfg.PrecompileWorkerImage,
			ArtifactStore: runtime_controllers.ArtifactStoreConfig{
				BaseURL: cfg.PrecompileArtifactBaseURL,
				Env: []corev1.EnvVar{
					{Name: "NATS_URL", Value: precompileNatsURL},
					{Name: "OBJECT_STORE_REPLICAS", Value: strconv.FormatUint(uint64(cfg.PrecompileArtifactReplicas), 10)},
				},
			},
			Target:             cfg.PrecompileTarget,
			WasmtimeVersion:    cfg.PrecompileWasmtimeVersion,
			InsecureRegistries: cfg.PrecompileInsecureRegistries,
		}).SetupWithManager(mgr); err != nil {
			return nil, err
		}

		// GC reads/deletes objects from the same precompiled-artifacts bucket
		// the Worker Jobs write to, so it needs its own connection to
		// precompileNatsURL whenever that differs from the operator's own
		// (Scheduler) connection.
		gcNatsConn := nc
		if precompileNatsURL != cfg.NatsURL {
			gcNatsConn, err = wasmbus.NatsConnect(precompileNatsURL, cfg.NatsOptions...)
			if err != nil {
				return nil, err
			}
		}

		if err = mgr.Add(&runtime_controllers.PrecompileGC{
			Reader:      mgr.GetClient(),
			NatsConn:    gcNatsConn,
			BaseURL:     cfg.PrecompileArtifactBaseURL,
			Interval:    cfg.PrecompileGCInterval,
			GracePeriod: cfg.PrecompileGCGracePeriod,
		}); err != nil {
			return nil, err
		}
	}

	if err = (&runtime_controllers.HostReconciler{
		Client:                  mgr.GetClient(),
		Scheme:                  mgr.GetScheme(),
		Bus:                     bus,
		UnreachableTimeout:      cfg.HeartbeatTTL,
		CPUThreshold:            cfg.HostCPUThreshold,
		MemoryThreshold:         cfg.HostMemoryThreshold,
		OperatorNamespace:       cfg.Namespace,
		MaxConcurrentReconciles: cfg.WorkloadConcurrency,
		RetiredHosts:            retiredHosts,
	}).SetupWithManager(mgr); err != nil {
		return nil, err
	}

	if err = (&runtime_controllers.HostPodReconciler{
		Client:            mgr.GetClient(),
		Scheme:            mgr.GetScheme(),
		OperatorNamespace: cfg.Namespace,
		RetiredHosts:      retiredHosts,
	}).SetupWithManager(mgr); err != nil {
		return nil, err
	}

	if err = (&runtime_controllers.WorkloadReconciler{
		Client:                  mgr.GetClient(),
		Scheme:                  mgr.GetScheme(),
		Bus:                     bus,
		Recorder:                mgr.GetEventRecorder("workload-controller"),
		OperatorNamespace:       cfg.Namespace,
		AllowSharedHosts:        cfg.AllowSharedHosts,
		MaxConcurrentReconciles: cfg.WorkloadConcurrency,
	}).SetupWithManager(mgr); err != nil {
		return nil, err
	}

	if err = (&runtime_controllers.WorkloadReplicaSetReconciler{
		Client: mgr.GetClient(),
		Scheme: mgr.GetScheme(),
	}).SetupWithManager(mgr); err != nil {
		return nil, err
	}

	if err = (&runtime_controllers.WorkloadDeploymentReconciler{
		Client:                    mgr.GetClient(),
		Scheme:                    mgr.GetScheme(),
		PrecompileTarget:          cfg.PrecompileTarget,
		PrecompileWasmtimeVersion: cfg.PrecompileWasmtimeVersion,
		MaxConcurrentReconciles:   cfg.WorkloadConcurrency,
	}).SetupWithManager(mgr); err != nil {
		return nil, err
	}

	if err = (&runtime_controllers.WorkloadRouteReconciler{
		Client:            mgr.GetClient(),
		Scheme:            mgr.GetScheme(),
		OperatorNamespace: cfg.Namespace,
	}).SetupWithManager(mgr); err != nil {
		return nil, err
	}

	return &EmbeddedOperator{
		Bus:      bus,
		NatsConn: nc,
	}, nil
}
