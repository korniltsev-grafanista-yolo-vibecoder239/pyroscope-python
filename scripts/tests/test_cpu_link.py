"""Check an installed, unstripped wheel without activating the CPU profiler."""

import os
import re
import subprocess
import sys

# Resolve all extension references on import, including normally lazy symbols.
sys.setdlopenflags(os.RTLD_NOW | os.RTLD_LOCAL)
import pyroscope
import pyroscope._native


symbols = subprocess.check_output(
    ["nm", "-C", pyroscope._native.__file__], text=True
)
assert re.search(r"\b[Tt]\s+_?PyInit__stack$", symbols, re.MULTILINE), (
    "CPU module was omitted from the linked extension"
)
assert re.search(r"\b[Tt]\s+.*Datadog::Sampler::start\(\)", symbols), (
    "CPU sampler was omitted from the linked extension"
)
unresolved = [
    line for line in symbols.splitlines()
    if re.search(r"\bU\s", line)
    and any(name in line for name in ("Datadog::", "Echion", "ddog_"))
]
assert not unresolved, unresolved
assert "ddtrace.internal.datadog.profiling.stack" not in sys.modules
assert "_stack" not in sys.modules
print("CPU sampler is linked; extension imports without activating the sampler")
