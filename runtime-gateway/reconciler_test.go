package main

import (
	"context"
	"errors"
	"testing"

	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/apimachinery/pkg/runtime"
	"k8s.io/apimachinery/pkg/types"
	ctrl "sigs.k8s.io/controller-runtime"
	"sigs.k8s.io/controller-runtime/pkg/client/fake"

	"go.wasmcloud.dev/runtime-operator/v2/api/condition"
	runtimev1alpha1 "go.wasmcloud.dev/runtime-operator/v2/api/runtime/v1alpha1"
)

// stubHostRegistry records every Register/Deregister call so a test can
// assert which (if any) the reconciler made.
type stubHostRegistry struct {
	registered   []string
	deregistered []string
}

func (r *stubHostRegistry) RegisterHost(_ context.Context, hostID, _ string, _ int) error {
	r.registered = append(r.registered, hostID)
	return nil
}

func (r *stubHostRegistry) DeregisterHost(_ context.Context, hostID string) error {
	r.deregistered = append(r.deregistered, hostID)
	return nil
}

// TestHostReconciler_RegistryActionsByReadyState pins down the user-facing
// contract: a Host going to Ready=Unknown — most often caused by NATS
// pressure, not the host actually being down — must NOT trigger
// DeregisterHost, so workloads on that host stay reachable. Ready=True and
// Ready=False are covered for completeness.
func TestHostReconciler_RegistryActionsByReadyState(t *testing.T) {
	cases := []struct {
		name             string
		readyCondition   condition.Condition
		wantRegistered   bool
		wantDeregistered bool
	}{
		{
			name:             "Ready=Unknown leaves the registry untouched so workloads remain accessible",
			readyCondition:   condition.UnknownCondition(condition.TypeReady, "TestUnknown", "simulated"),
			wantRegistered:   false,
			wantDeregistered: false,
		},
		{
			name:           "Ready=True registers the host endpoint",
			readyCondition: condition.ReadyCondition(condition.TypeReady),
			wantRegistered: true,
		},
		{
			name:             "Ready=False deregisters the host endpoint",
			readyCondition:   condition.ErrorCondition(condition.TypeReady, "TestFalse", errors.New("host down")),
			wantDeregistered: true,
		},
	}

	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			scheme := runtime.NewScheme()
			if err := runtimev1alpha1.AddToScheme(scheme); err != nil {
				t.Fatalf("add scheme: %v", err)
			}

			host := &runtimev1alpha1.Host{
				ObjectMeta: metav1.ObjectMeta{
					Name:       "host-1",
					Namespace:  "wasmcloud-system",
					Finalizers: []string{gatewayHostFinalizerName},
				},
				HostID:   "host-1-id",
				Hostname: "10.0.0.1",
				HTTPPort: 8080,
			}
			host.Status.SetConditions(tc.readyCondition)

			c := fake.NewClientBuilder().
				WithScheme(scheme).
				WithObjects(host).
				WithStatusSubresource(&runtimev1alpha1.Host{}).
				Build()

			reg := &stubHostRegistry{}
			r := &HostReconciler{Client: c, Registry: reg}

			_, err := r.Reconcile(context.Background(), ctrl.Request{
				NamespacedName: types.NamespacedName{Name: host.Name, Namespace: host.Namespace},
			})
			if err != nil {
				t.Fatalf("Reconcile: %v", err)
			}

			if tc.wantRegistered && len(reg.registered) == 0 {
				t.Errorf("expected RegisterHost to be called, got none")
			}
			if !tc.wantRegistered && len(reg.registered) > 0 {
				t.Errorf("did not expect RegisterHost, got %v", reg.registered)
			}
			if tc.wantDeregistered && len(reg.deregistered) == 0 {
				t.Errorf("expected DeregisterHost to be called, got none")
			}
			if !tc.wantDeregistered && len(reg.deregistered) > 0 {
				t.Errorf("did not expect DeregisterHost, got %v", reg.deregistered)
			}
		})
	}
}
