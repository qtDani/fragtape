"""List running processes on Windows without third-party packages."""
from __future__ import annotations

import ctypes
import sys
from ctypes import wintypes

TH32CS_SNAPPROCESS = 0x00000002
PROCESS_QUERY_LIMITED_INFORMATION = 0x1000
MAX_PATH = 260


class PROCESSENTRY32W(ctypes.Structure):
    _fields_ = [
        ("dwSize", wintypes.DWORD),
        ("cntUsage", wintypes.DWORD),
        ("th32ProcessID", wintypes.DWORD),
        ("th32DefaultHeapID", ctypes.c_size_t),
        ("th32ModuleID", wintypes.DWORD),
        ("cntThreads", wintypes.DWORD),
        ("th32ParentProcessID", wintypes.DWORD),
        ("pcPriClassBase", ctypes.c_long),
        ("dwFlags", wintypes.DWORD),
        ("szExeFile", wintypes.WCHAR * MAX_PATH),
    ]


_k32 = None


def _kernel32():
    global _k32
    if _k32 is None:
        k = ctypes.WinDLL("kernel32", use_last_error=True)
        k.CreateToolhelp32Snapshot.restype = wintypes.HANDLE
        k.CreateToolhelp32Snapshot.argtypes = [wintypes.DWORD, wintypes.DWORD]
        k.Process32FirstW.argtypes = [wintypes.HANDLE, ctypes.POINTER(PROCESSENTRY32W)]
        k.Process32NextW.argtypes = [wintypes.HANDLE, ctypes.POINTER(PROCESSENTRY32W)]
        k.CloseHandle.argtypes = [wintypes.HANDLE]
        k.OpenProcess.restype = wintypes.HANDLE
        k.OpenProcess.argtypes = [wintypes.DWORD, wintypes.BOOL, wintypes.DWORD]
        k.QueryFullProcessImageNameW.argtypes = [
            wintypes.HANDLE, wintypes.DWORD, wintypes.LPWSTR, ctypes.POINTER(wintypes.DWORD)
        ]
        _k32 = k
    return _k32


class Snapshot(set):
    """Set of lower-cased exe names, plus lazy full-path lookup per name."""

    def __init__(self, pids: dict[str, list[int]] | None = None, paths: dict[str, list[str]] | None = None):
        super().__init__((pids or {}).keys() | (paths or {}).keys())
        self._pids = pids or {}
        self._paths = dict(paths or {})

    def pids(self, name: str) -> list[int]:
        return list(self._pids.get(name, []))

    def paths(self, name: str) -> list[str]:
        """Lower-cased full paths (with / separators) of processes called ``name``."""
        if name not in self._paths:
            self._paths[name] = [p for p in (_image_path(pid) for pid in self._pids.get(name, [])) if p]
        return self._paths[name]


def _image_path(pid: int) -> str | None:
    k = _kernel32()
    h = k.OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, False, pid)
    if not h:
        return None
    try:
        buf = ctypes.create_unicode_buffer(1024)
        size = wintypes.DWORD(len(buf))
        if k.QueryFullProcessImageNameW(h, 0, buf, ctypes.byref(size)):
            return buf.value.replace("\\", "/").lower()
        return None
    finally:
        k.CloseHandle(h)


def main_window(pids: list[int]) -> tuple[str, str] | None:
    """(title, class) of the biggest visible top-level window owned by ``pids``."""
    if sys.platform != "win32" or not pids:
        return None
    u = ctypes.WinDLL("user32", use_last_error=True)
    wanted = set(pids)
    best: list = [0, None]
    rect = wintypes.RECT()
    pid = wintypes.DWORD()

    @ctypes.WINFUNCTYPE(wintypes.BOOL, wintypes.HWND, wintypes.LPARAM)
    def visit(hwnd, _):
        if not u.IsWindowVisible(hwnd) or u.GetWindow(hwnd, 4):  # GW_OWNER: skip owned popups
            return True
        u.GetWindowThreadProcessId(hwnd, ctypes.byref(pid))
        if pid.value not in wanted or not u.GetWindowRect(hwnd, ctypes.byref(rect)):
            return True
        area = (rect.right - rect.left) * (rect.bottom - rect.top)
        if area > best[0]:
            title = ctypes.create_unicode_buffer(512)
            cls = ctypes.create_unicode_buffer(256)
            u.GetWindowTextW(hwnd, title, 512)
            u.GetClassNameW(hwnd, cls, 256)
            best[0], best[1] = area, (title.value, cls.value)
        return True

    u.EnumWindows(visit, 0)
    return best[1]


class _MONITORINFO(ctypes.Structure):
    _fields_ = [("cbSize", wintypes.DWORD), ("rcMonitor", wintypes.RECT),
                ("rcWork", wintypes.RECT), ("dwFlags", wintypes.DWORD)]


def monitor_size(pids: list[int] | None = None) -> tuple[int, int]:
    """Pixel size of the monitor the game's window is on (else the primary monitor).

    The calling process should be DPI aware (see ``make_dpi_aware``) to get real pixels.
    """
    if sys.platform != "win32":
        return (1920, 1080)
    u = ctypes.WinDLL("user32", use_last_error=True)
    u.MonitorFromWindow.restype = wintypes.HANDLE
    u.MonitorFromPoint.restype = wintypes.HANDLE
    u.MonitorFromWindow.argtypes = [wintypes.HWND, wintypes.DWORD]
    u.GetMonitorInfoW.argtypes = [wintypes.HANDLE, ctypes.POINTER(_MONITORINFO)]
    hmon = None
    hwnd = _main_hwnd(pids or [])
    if hwnd:
        hmon = u.MonitorFromWindow(hwnd, 2)  # MONITOR_DEFAULTTONEAREST
    if not hmon:
        hmon = u.MonitorFromPoint(wintypes.POINT(0, 0), 1)  # MONITOR_DEFAULTTOPRIMARY
    info = _MONITORINFO()
    info.cbSize = ctypes.sizeof(info)
    if hmon and u.GetMonitorInfoW(hmon, ctypes.byref(info)):
        r = info.rcMonitor
        return (r.right - r.left, r.bottom - r.top)
    return (u.GetSystemMetrics(0), u.GetSystemMetrics(1))


def _main_hwnd(pids: list[int]):
    if not pids:
        return None
    u = ctypes.WinDLL("user32", use_last_error=True)
    wanted = set(pids)
    best: list = [0, None]
    rect = wintypes.RECT()
    pid = wintypes.DWORD()

    @ctypes.WINFUNCTYPE(wintypes.BOOL, wintypes.HWND, wintypes.LPARAM)
    def visit(hwnd, _):
        if not u.IsWindowVisible(hwnd) or u.GetWindow(hwnd, 4):
            return True
        u.GetWindowThreadProcessId(hwnd, ctypes.byref(pid))
        if pid.value in wanted and u.GetWindowRect(hwnd, ctypes.byref(rect)):
            area = (rect.right - rect.left) * (rect.bottom - rect.top)
            if area > best[0]:
                best[0], best[1] = area, hwnd
        return True

    u.EnumWindows(visit, 0)
    return best[1]


def make_dpi_aware() -> None:
    if sys.platform == "win32":
        try:
            ctypes.windll.user32.SetProcessDpiAwarenessContext(ctypes.c_void_p(-4))  # PER_MONITOR_AWARE_V2
        except Exception:
            pass


def gpu_names() -> list[str]:
    """Names of the display adapters ("NVIDIA GeForce RTX 5080", ...)."""
    if sys.platform != "win32":
        return []

    class DISPLAY_DEVICEW(ctypes.Structure):
        _fields_ = [("cb", wintypes.DWORD), ("DeviceName", wintypes.WCHAR * 32), ("DeviceString", wintypes.WCHAR * 128),
                    ("StateFlags", wintypes.DWORD), ("DeviceID", wintypes.WCHAR * 128), ("DeviceKey", wintypes.WCHAR * 128)]

    u = ctypes.WinDLL("user32")
    names: list[str] = []
    i = 0
    while True:
        dd = DISPLAY_DEVICEW()
        dd.cb = ctypes.sizeof(dd)
        if not u.EnumDisplayDevicesW(None, i, ctypes.byref(dd), 0):
            break
        if dd.DeviceString and dd.DeviceString not in names:
            names.append(dd.DeviceString)
        i += 1
    return names


def obs_window_spec(exe: str, window: tuple[str, str] | None = None) -> str:
    """Window string used by OBS capture sources: "title:class:exe" with ':' and '#' escaped."""
    def enc(s: str) -> str:
        return s.replace("#", "#22").replace(":", "#3A")

    title, cls = window or ("", "")
    return f"{enc(title)}:{enc(cls)}:{enc(exe)}"


def lower_thread_priority() -> None:
    """Run the calling thread at idle priority so background work never competes with a game."""
    if sys.platform == "win32":
        try:
            k = _kernel32()
            k.SetThreadPriority(k.GetCurrentThread(), -15)  # THREAD_PRIORITY_IDLE
        except Exception:
            pass


def running_processes() -> Snapshot:
    """Return the running processes (exe names are lower-cased)."""
    if sys.platform != "win32":
        return Snapshot()
    k = _kernel32()
    snap = k.CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0)
    if not snap or snap == wintypes.HANDLE(-1).value:
        return Snapshot()
    pids: dict[str, list[int]] = {}
    try:
        entry = PROCESSENTRY32W()
        entry.dwSize = ctypes.sizeof(PROCESSENTRY32W)
        ok = k.Process32FirstW(snap, ctypes.byref(entry))
        while ok:
            pids.setdefault(entry.szExeFile.lower(), []).append(entry.th32ProcessID)
            ok = k.Process32NextW(snap, ctypes.byref(entry))
    finally:
        k.CloseHandle(snap)
    return Snapshot(pids)
