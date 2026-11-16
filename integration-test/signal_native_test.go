package integrationtest

import (
	"fmt"
	"testing"
	"time"

	"pyroscope-python-integration-test/dockertest"
)

func TestPythonSignalProfilerNativeCPU(t *testing.T) {
	skipUnlessPythonSupportsSignal(t)
	_, minor := pythonVersion(t)
	for _, timer := range []string{"ITimer", "CTimer"} {
		t.Run(timer, func(t *testing.T) {
			wheelDir := ensureWheel(t)
			net := dockertest.CreateNetwork(t)
			pyroscopeURL := startPyroscope(t, net)
			appName := fmt.Sprintf("pyroscopers.python.test.native.%d", time.Now().UnixNano())
			workload := startPythonTestContainer(t, net, wheelDir, "signal_native_workload.py", map[string]string{
				"PYROSCOPE_APPLICATION_NAME": appName,
				"SIGNAL_TIMER":               timer,
			})
			requireContainerExit(t, workload, 0, 2*time.Minute)
			cfg := profileConfig{onCPU: true, signalTimer: timer}
			pythonPrefix := threadStackPrefix(cfg, minor)
			// The native worker starts before configure(), so on 3.10 its
			// bootstrap has no CTimer wrapper. On 3.11+ neither has a wrapper.
			nativePrefix := threadStackPrefix(profileConfig{onCPU: true, signalTimer: "ITimer"}, minor)
			// Each worker uses six CPU seconds. Allow 15 percentage points
			// of sampling/timer noise, while requiring at least 90% of *all*
			// weight in the two complete stacks, including GIL-releasing work.
			// CTimer runs both concurrently and must account for 80% of their
			// CPU time. ITimer runs the phases sequentially, but its coarse
			// process CPU ticks can still lose expirations under scheduler load;
			// require at least 480 samples with the same full-stack shape.
			minTotal := int64(4800 * time.Millisecond)
			if timer == "CTimer" {
				minTotal = int64(9600 * time.Millisecond)
			}
			selector := fmt.Sprintf(`{service_name="%s"}`, appName)
			requireProfileShape(t, workload, pyroscopeURL, cpuProfileTypeID, selector, profileShape{
				stacks: []stackShare{
					{pythonPrefix + "python_work", 0.35, 0.65},
					{nativePrefix + "native_work", 0.35, 0.65},
				},
				minTotal: minTotal, minCoverage: 0.90,
			}, time.Minute)
		})
	}
}
