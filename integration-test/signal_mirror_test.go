package integrationtest

import (
	"fmt"
	"testing"
	"time"

	"pyroscope-python-integration-test/dockertest"
)

func TestPythonSignalNativeAndPythonThreadLifetimes(t *testing.T) {
	skipUnlessPythonSupportsSignal(t)
	wheelDir := ensureWheel(t)
	net := dockertest.CreateNetwork(t)
	url := startPyroscope(t, net)
	_, minor := pythonVersion(t)
	for _, timer := range []string{"ITimer", "CTimer"} {
		t.Run(timer, func(t *testing.T) {
			budget := "2"
			if timer == "ITimer" {
				// ITIMER_PROF has tick accounting/coalescing losses without an
				// overrun count, even with a trivial native handler. Observe it
				// longer to keep the same >=600-period statistical floor.
				budget = "6"
			}
			app := fmt.Sprintf("signal.mirror.%s.%d", timer, time.Now().UnixNano())
			workload := startPythonTestContainer(t, net, wheelDir, "signal_mirror_workload.py", map[string]string{
				"PYROSCOPE_APPLICATION_NAME": app, "TIMER": timer, "CPU_SECONDS": budget,
			})
			requireContainerExit(t, workload, 0, 2*time.Minute)
			for session := 0; session < 2; session++ {
				var stacks []stackShare
				for _, worker := range []string{"preexisting_worker", "new_worker"} {
					prefix := threadStackPrefix(profileConfig{signalTimer: timer}, minor)
					if minor == 10 && timer == "CTimer" && session == 0 && worker == "preexisting_worker" {
						// 3.10's bootstrap wrapper cannot wrap an already-entered
						// bootstrap. The second session inherits the installed hook.
						prefix = threadStackPrefix(profileConfig{}, minor)
					}
					for _, leaf := range []string{"python_leaf", "native_leaf"} {
						// Equal CPU budgets predict 25% per leaf. Allow timer
						// delivery bias and noise, while all four must dominate.
						stacks = append(stacks, stackShare{prefix + worker + ";" + leaf, 0.15, 0.35})
					}
				}
				selector := fmt.Sprintf(`{service_name="%s",session="%d"}`, app, session)
				requireProfileShape(t, workload, url, cpuProfileTypeID, selector, profileShape{
					stacks: stacks, minTotal: int64(6 * time.Second), minCoverage: 0.90,
				}, 45*time.Second)
			}
		})
	}
}
