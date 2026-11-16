package integrationtest

import (
	"fmt"
	"testing"
	"time"

	"pyroscope-python-integration-test/dockertest"
)

func TestPythonSignalProfilerPublishedStates(t *testing.T) {
	skipUnlessPythonSupportsSignal(t)
	wheelDir := ensureWheel(t)
	net := dockertest.CreateNetwork(t)
	url := startPyroscope(t, net)
	_, minor := pythonVersion(t)
	for _, timer := range []string{"ITimer", "CTimer"} {
		for _, scenario := range []string{"main", "preexisting", "new", "churn", "restart", "raw"} {
			if timer == "CTimer" && scenario == "raw" {
				continue // CTimer registers threading.Thread, not _thread/foreign threads.
			}
			t.Run(timer+"/"+scenario, func(t *testing.T) {
				name := fmt.Sprintf("published.%s.%s.%d", timer, scenario, time.Now().UnixNano())
				workload := startPythonTestContainer(t, net, wheelDir, "signal_publisher_workload.py", map[string]string{
					"SCENARIO": scenario, "SIGNAL_TIMER": timer, "PYROSCOPE_APPLICATION_NAME": name,
				})
				requireContainerExit(t, workload, 0, time.Minute)
				prefix := threadStackPrefix(profileConfig{signalTimer: timer}, minor)
				if minor == 10 && timer == "CTimer" && scenario == "preexisting" {
					// Already running before the 3.10 bootstrap wrapper was installed.
					prefix = threadStackPrefix(profileConfig{signalTimer: "ITimer"}, minor)
				}
				stacks := map[string]string{
					"main":        "<module>",
					"preexisting": prefix + "preexisting_work;thread_work;native_cpu",
					"new":         prefix + "thread_work;native_cpu",
					"churn":       prefix + "thread_work;native_cpu",
					"restart":     "<module>;native_cpu",
					"raw":         "raw_work;native_cpu",
				}
				minTotal := int64(1600 * time.Millisecond)
				if timer == "ITimer" {
					minTotal = int64(3 * time.Second)
				}
				// CTimer spends 2 thread CPU seconds in native code: allow 20% timer
				// noise in absolute weight. ITimer runs for 6 CPU seconds and requires
				// >=300 deliveries; it has no overrun count and can systematically
				// undercount native CPU, even with a minimal C-only signal handler.
				// Both require >=90% of ALL observed weight on the complete stack,
				// allowing 10% startup/teardown/sampling noise. Drops are included;
				// missing native work or a few matching samples cannot pass.
				requireProfileShape(t, workload, url, cpuProfileTypeID, fmt.Sprintf(`{service_name="%s"}`, name), profileShape{
					stacks:   []stackShare{{stacks[scenario], 0.90, 1}},
					minTotal: minTotal, minCoverage: 0.90,
				}, 45*time.Second)
			})
		}
	}
}
