package main

import (
	"context"
	"net/http"
	"testing"

	"k8s.io/apimachinery/pkg/util/sets"
)

// stubFallback records which fallback method was called and with what hostname.
type stubFallback struct {
	calledMethod string
	calledHost   string
}

func (f *stubFallback) InvalidHostname(hostname string) (string, string) {
	f.calledMethod = "InvalidHostname"
	f.calledHost = hostname
	return "http", "fallback:9999"
}

func (f *stubFallback) NoWorkloads(hostname string) (string, string) {
	f.calledMethod = "NoWorkloads"
	f.calledHost = hostname
	return "http", "fallback:9999"
}

// newTestTracker creates a HostTracker pre-populated with a single host+workload.
func newTestTracker(fb Fallback) *HostTracker {
	ht := &HostTracker{
		Fallback:  fb,
		hosts:     map[string]string{"host-1": "10.0.0.1:8080"},
		hostnames: map[string]sets.Set[string]{"app.example.com": sets.New("workload-1")},
		workloads: map[string]string{"workload-1": "host-1"},
	}
	return ht
}

func TestResolve_XRouteHostRoutesToCorrectWorkload(t *testing.T) {
	fb := &stubFallback{}
	ht := newTestTracker(fb)

	req, _ := http.NewRequest("GET", "http://gateway.local/path", nil)
	req.Host = "gateway.local" // original host — not registered
	req.Header.Set("X-Route-Host", "app.example.com")

	result := ht.Resolve(context.Background(), req)

	if result.Hostname != "10.0.0.1:8080" {
		t.Errorf("expected hostname 10.0.0.1:8080, got %s", result.Hostname)
	}
	if result.WorkloadID != "workload-1" {
		t.Errorf("expected workload-1, got %s", result.WorkloadID)
	}
	if result.Scheme != "http" {
		t.Errorf("expected scheme http, got %s", result.Scheme)
	}
}

func TestResolve_NormalHostRoutingWithoutXRouteHost(t *testing.T) {
	fb := &stubFallback{}
	ht := newTestTracker(fb)

	req, _ := http.NewRequest("GET", "http://app.example.com/path", nil)
	req.Host = "app.example.com"

	result := ht.Resolve(context.Background(), req)

	if result.Hostname != "10.0.0.1:8080" {
		t.Errorf("expected hostname 10.0.0.1:8080, got %s", result.Hostname)
	}
	if result.WorkloadID != "workload-1" {
		t.Errorf("expected workload-1, got %s", result.WorkloadID)
	}
}

func TestResolve_XRouteHostTakesPrecedenceOverHost(t *testing.T) {
	fb := &stubFallback{}
	ht := newTestTracker(fb)

	// Register a second workload on a different hostname
	ht.hosts["host-2"] = "10.0.0.2:8080"
	ht.hostnames["other.example.com"] = sets.New("workload-2")
	ht.workloads["workload-2"] = "host-2"

	req, _ := http.NewRequest("GET", "http://app.example.com/path", nil)
	req.Host = "app.example.com"
	req.Header.Set("X-Route-Host", "other.example.com")

	result := ht.Resolve(context.Background(), req)

	if result.Hostname != "10.0.0.2:8080" {
		t.Errorf("expected hostname 10.0.0.2:8080, got %s", result.Hostname)
	}
	if result.WorkloadID != "workload-2" {
		t.Errorf("expected workload-2, got %s", result.WorkloadID)
	}
}

func TestResolve_UnknownXRouteHostTriggersFallback(t *testing.T) {
	fb := &stubFallback{}
	ht := newTestTracker(fb)

	req, _ := http.NewRequest("GET", "http://gateway.local/path", nil)
	req.Host = "gateway.local"
	req.Header.Set("X-Route-Host", "unknown.example.com")

	result := ht.Resolve(context.Background(), req)

	if fb.calledMethod != "InvalidHostname" {
		t.Errorf("expected InvalidHostname fallback, got %s", fb.calledMethod)
	}
	if fb.calledHost != "unknown.example.com" {
		t.Errorf("expected fallback called with unknown.example.com, got %s", fb.calledHost)
	}
	if result.Hostname != "fallback:9999" {
		t.Errorf("expected fallback hostname, got %s", result.Hostname)
	}
}
