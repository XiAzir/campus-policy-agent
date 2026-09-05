import importlib.util
from pathlib import Path

import pytest


def test_real_e2e_uses_temporary_data_and_waits_for_child(monkeypatch):
    path = Path(__file__).parents[1] / "scripts" / "e2e_real.py"
    spec = importlib.util.spec_from_file_location("e2e_real", path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    seen = {}
    class Process:
        def __init__(self, command, **kwargs):
            seen.update(kwargs)
            self.stopped = False
        def poll(self):
            return 0 if self.stopped else None
        def terminate(self):
            self.stopped = True
        def wait(self, **kwargs):
            seen["waited"] = True
    def fail(**kwargs):
        raise RuntimeError("offline-test")
    monkeypatch.setattr(module.subprocess, "Popen", Process)
    monkeypatch.setattr(module.time, "sleep", lambda _: None)
    monkeypatch.setattr(module.httpx, "Client", fail)
    monkeypatch.setattr(module, "PORT", 0)
    monkeypatch.setenv("DATA_DIR", "must-not-use")
    with pytest.raises(RuntimeError, match="offline-test"):
        module.main()
    assert Path(seen["env"]["DATA_DIR"]).name.startswith("cpb-real-e2e-")
    assert not Path(seen["env"]["DATA_DIR"]).exists()
    assert seen["waited"]
