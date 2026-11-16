package integrationtest

import (
	"fmt"
	"testing"
	"time"

	"pyroscope-python-integration-test/dockertest"
)

func TestPythonSignalProfilerPythonAndNative(t *testing.T) {
	skipUnlessPythonSupportsSignal(t)
	wheelDir := ensureWheel(t)
	net := dockertest.CreateNetwork(t)
	pyroscopeURL := startPyroscope(t, net)
	_, minor := pythonVersion(t)
	for _, timer := range []string{"ITimer", "CTimer"} {
		t.Run(timer, func(t *testing.T) {
			appName := fmt.Sprintf("signal.compatibility.%s.%d", timer, time.Now().UnixNano())
			workload := startPythonTestContainer(t, net, wheelDir, "signal_compatibility_workload.py", map[string]string{
				"PYROSCOPE_APPLICATION_NAME": appName,
				"SIGNAL_TIMER":               timer,
			})
			requireContainerExit(t, workload, 0, time.Minute)
			prefix := threadStackPrefix(profileConfig{signalTimer: timer}, minor)
			// Four equal three-second thread CPU budgets, including preexisting
			// and new threads and C extension work with the GIL released.
			// Alternate short Python/native budgets to average coarse timer
			// quantization across both phases rather than sampling long phases
			// under different scheduling conditions.
			// Each full path must contribute 18–32%, with at least 95% coverage
			// and 600 periods at 100Hz overall. ITimer is coarse and coalesces
			// notifications, so do not require its total weight to calibrate a
			// CPU clock. The shape tolerance cannot hide a missing phase or worker.
			want := profileShape{minTotal: int64(6 * time.Second), minCoverage: 0.95}
			for _, worker := range []string{"before", "after"} {
				workerPrefix := prefix
				if minor == 10 && timer == "CTimer" && worker == "before" {
					// The already running bootstrap does not acquire the wrapper
					// installed at configure time on CPython 3.10.
					workerPrefix = threadStackPrefix(profileConfig{signalTimer: "ITimer"}, minor)
				}
				for _, leaf := range []string{"python_leaf", "native_leaf"} {
					want.stacks = append(want.stacks, stackShare{workerPrefix + worker + ";" + leaf, 0.18, 0.32})
				}
			}
			requireProfileShape(t, workload, pyroscopeURL, cpuProfileTypeID, fmt.Sprintf(`{service_name="%s"}`, appName), want, 45*time.Second)
		})
	}
}

func TestPythonSignalProfilerUnregisteredThread(t *testing.T) {
	skipUnlessPythonSupportsSignal(t)
	wheelDir := ensureWheel(t)
	net := dockertest.CreateNetwork(t)
	pyroscopeURL := startPyroscope(t, net)
	appName := fmt.Sprintf("signal.rawthread.%d", time.Now().UnixNano())
	workload := startPythonTestContainer(t, net, wheelDir, "signal_compatibility_workload.py", map[string]string{
		"PYROSCOPE_APPLICATION_NAME": appName,
		"SIGNAL_TIMER":               "ITimer",
		"MODE":                       "raw",
	})
	requireContainerExit(t, workload, 0, time.Minute)
	// ITimer also samples _thread threads, which have no threading hooks.
	// Equal CPU budgets should give half the weight to each complete path.
	requireProfileShape(t, workload, pyroscopeURL, cpuProfileTypeID, fmt.Sprintf(`{service_name="%s"}`, appName), profileShape{
		stacks: []stackShare{
			{"raw_worker;python_leaf", 0.40, 0.60},
			{"raw_worker;native_leaf", 0.40, 0.60},
		},
		minTotal: int64(3 * time.Second), minCoverage: 0.95,
	}, 45*time.Second)
}
