package integrationtest

import (
	"fmt"
	"testing"
	"time"

	"pyroscope-python-integration-test/dockertest"
)

func TestSignalThreadBindings(t *testing.T) {
	skipUnlessPythonSupportsSignal(t)
	_, minor := pythonVersion(t)
	for _, tc := range []struct{ timer, scenario string }{
		{"ITimer", "threading"}, {"CTimer", "threading"}, {"ITimer", "foreign"},
	} {
		t.Run(tc.timer+"/"+tc.scenario, func(t *testing.T) {
			net := dockertest.CreateNetwork(t)
			server := startPyroscope(t, net)
			app := fmt.Sprintf("signal.bindings.%d", time.Now().UnixNano())
			canary := randomHex(t, 16)
			workload := startPythonTestContainer(t, net, ensureWheel(t), "signal_binding_workload.py", map[string]string{
				"PYROSCOPE_APPLICATION_NAME": app, "CANARY": canary,
				"SIGNAL_TIMER": tc.timer, "SCENARIO": tc.scenario,
			})
			requireContainerExit(t, workload, 0, time.Minute)
			prefix := threadStackPrefix(profileConfig{signalTimer: tc.timer}, minor)
			// The old 3.10 thread began before CTimer wrapped _bootstrap_inner.
			oldPrefix := threadStackPrefix(profileConfig{}, minor)
			stacks := []stackShare{
				{oldPrefix + "preexisting;native_leaf", .35, .65},
				{prefix + "new_thread;python_leaf", .35, .65},
			}
			minimum := int64(3 * time.Second)
			if tc.scenario == "foreign" {
				stacks = []stackShare{
					{oldPrefix + "preexisting;native_leaf", .15, .35},
					{prefix + "new_thread;python_leaf", .15, .35},
					{"raw_thread;native_leaf", .15, .35},
					{"foreign_thread;native_leaf", .15, .35},
				}
				minimum = int64(6 * time.Second)
			}
			// Each worker burns four thread CPU seconds. Keep >=300/600 observed
			// periods even with ITimer undercounting. Permit timer quantization,
			// native batch overshoot, startup, and interpreter-version overhead, but
			// require all complete stacks and >=90% of *all* weight (including drops).
			requireProfileShape(t, workload, server, cpuProfileTypeID,
				fmt.Sprintf(`{service_name="%s",canary="%s"}`, app, canary),
				profileShape{stacks: stacks, minTotal: minimum, minCoverage: .90}, 2*time.Minute)
		})
	}
}
