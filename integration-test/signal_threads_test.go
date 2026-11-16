package integrationtest

import (
	"fmt"
	"testing"
	"time"

	"pyroscope-python-integration-test/dockertest"
)

func TestPythonSignalProfilerNativeWork(t *testing.T) {
	skipUnlessPythonSupportsSignal(t)
	wheelDir := ensureWheel(t)
	net := dockertest.CreateNetwork(t)
	pyroscopeURL := startPyroscope(t, net)
	_, minor := pythonVersion(t)
	for _, timer := range []string{"ITimer", "CTimer"} {
		for _, scenario := range []string{"main", "preexisting", "foreign"} {
			if timer == "CTimer" && scenario == "foreign" {
				continue // CTimer covers threads registered by the Python hooks.
			}
			t.Run(timer+"/"+scenario, func(t *testing.T) {
				appName := fmt.Sprintf("pyroscopers.python.test.native.%d", time.Now().UnixNano())
				workload := startPythonTestContainer(t, net, wheelDir, "signal_threads_workload.py", map[string]string{
					"PYROSCOPE_APPLICATION_NAME": appName,
					"SCENARIO":                   scenario,
					"SIGNAL_TIMER":               timer,
				})
				requireContainerExit(t, workload, 0, time.Minute)
				stack := "<module>;main;native_work"
				switch scenario {
				case "preexisting":
					name := "main.<locals>.preexisting"
					if minor == 10 {
						name = "preexisting"
					}
					// This worker entered _bootstrap_inner before hooks existed.
					stack = threadStackPrefix(profileConfig{}, minor) + name + ";native_work"
				case "foreign":
					stack = "native_callback;native_work"
				}
				// The worker uses 6 thread-CPU seconds, almost all in one native
				// hash leaf. ITimer rounds its interval and coalesces deliveries,
				// particularly under scheduler contention: gather >=200 periods.
				// CTimer preserves overruns, so also require 80% of the CPU time.
				// Both require >=90% on the complete path, allowing startup and
				// loop bookkeeping; drops remain in the denominator.
				minimum := int64(2 * time.Second)
				if timer == "CTimer" {
					minimum = int64(4800 * time.Millisecond)
				}
				selector := fmt.Sprintf(`{service_name="%s"}`, appName)
				requireProfileShape(t, workload, pyroscopeURL, cpuProfileTypeID, selector, profileShape{
					stacks: []stackShare{{stack, 0.90, 1}}, minTotal: minimum, minCoverage: 0.90,
				}, time.Minute)
			})
		}
	}
}
