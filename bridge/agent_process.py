"""Bound CLI lifetimes, including child processes, without logging private IO."""
import os
import signal
import subprocess

# Windows has no POSIX process groups: os.killpg does not exist and
# start_new_session is ignored, and `taskkill /T` misses descendants whose
# parent already exited (uv's python.exe is such a trampoline). A Job Object is
# the Windows equivalent: the agent starts suspended, joins a job, then resumes,
# so every descendant is in the job before it can run, and TerminateJobObject
# ends them all. (Windows adaptation for the evaluation; POSIX path unchanged.)
_WINDOWS = os.name == "nt"
if _WINDOWS:
    import ctypes
    _kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
    _kernel32.CreateJobObjectW.restype = ctypes.c_void_p
    _kernel32.AssignProcessToJobObject.argtypes = (ctypes.c_void_p, ctypes.c_void_p)
    _kernel32.TerminateJobObject.argtypes = (ctypes.c_void_p, ctypes.c_uint)
    _kernel32.CloseHandle.argtypes = (ctypes.c_void_p,)
    _ntdll = ctypes.WinDLL("ntdll")
    _ntdll.NtResumeProcess.argtypes = (ctypes.c_void_p,)
    _CREATE_SUSPENDED = 0x00000004


def _start(command, kwargs):
    if not _WINDOWS:
        return subprocess.Popen(command, start_new_session=True, **kwargs), None
    flags = kwargs.pop("creationflags", 0) | subprocess.CREATE_NEW_PROCESS_GROUP | _CREATE_SUSPENDED
    process = subprocess.Popen(command, creationflags=flags, **kwargs)
    job = _kernel32.CreateJobObjectW(None, None)
    if not job or not _kernel32.AssignProcessToJobObject(job, int(process._handle)):
        process.kill()
        raise OSError("Could not contain the agent process in a job object")
    _ntdll.NtResumeProcess(int(process._handle))
    return process, job


def _kill_tree(process, job):
    if job:
        _kernel32.TerminateJobObject(job, 1)
        return
    try:
        os.killpg(process.pid, signal.SIGKILL)
    except ProcessLookupError:
        pass


def run_agent(command, *, timeout, input=None, text=True, **kwargs):
    process, job = _start(command, dict(stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                        stderr=subprocess.PIPE, text=text, **kwargs))
    try:
        with process:
            try:
                stdout, stderr = process.communicate(input, timeout=timeout)
            except subprocess.TimeoutExpired:
                # Kill the entire request process group before considering fallback.
                # CLI tools can otherwise keep acting after their parent times out.
                _kill_tree(process, job)
                stdout, stderr = process.communicate()
                raise subprocess.TimeoutExpired(command, timeout, output=stdout, stderr=stderr) from None
            return subprocess.CompletedProcess(command, process.returncode, stdout, stderr)
    finally:
        if job:
            _kernel32.CloseHandle(job)


def read_only_events(output, provider):
    """Fail closed when a failed CLI may already have taken external actions."""
    import json
    seen = False
    for line in (output or "").splitlines():
        try:
            event = json.loads(line)
            kind = event["type"]
            if provider == "codex":
                if kind.startswith("item."):
                    if event["item"]["type"] not in ("web_search", "agent_message", "reasoning"):
                        return False
                elif kind not in ("thread.started", "turn.started", "turn.completed", "turn.failed", "error"):
                    return False
            else:
                if kind == "assistant":
                    for block in event["message"]["content"]:
                        if block.get("type") == "tool_use" and block.get("name") not in ("WebSearch", "WebFetch", "StructuredOutput"):
                            return False
                        if block.get("type") not in ("text", "thinking", "redacted_thinking", "tool_use"):
                            return False
                elif kind not in ("system", "user", "result", "rate_limit_event"):
                    return False
            seen = True
        except (ValueError, KeyError, TypeError, AttributeError):
            return False
    return seen
