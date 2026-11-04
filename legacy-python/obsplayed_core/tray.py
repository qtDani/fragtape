"""Notification-area (tray) icon and global hotkey, with plain Win32 calls."""
from __future__ import annotations

import ctypes
import struct
import threading
from ctypes import wintypes
from pathlib import Path
from typing import Callable

WM_DESTROY, WM_COMMAND, WM_HOTKEY, WM_APP = 0x0002, 0x0111, 0x0312, 0x8000
WM_LBUTTONUP, WM_RBUTTONUP, WM_LBUTTONDBLCLK = 0x0202, 0x0205, 0x0203
WM_TRAY, WM_REFRESH, WM_QUIT_APP = WM_APP + 1, WM_APP + 2, WM_APP + 3
NIM_ADD, NIM_MODIFY, NIM_DELETE = 0, 1, 2
NIF_MESSAGE, NIF_ICON, NIF_TIP, NIF_INFO = 1, 2, 4, 0x10
MOD_ALT, MOD_CONTROL, MOD_SHIFT, MOD_NOREPEAT = 1, 2, 4, 0x4000
HOTKEY_ID = 1

LRESULT = ctypes.c_ssize_t
WNDPROC = ctypes.WINFUNCTYPE(LRESULT, wintypes.HWND, wintypes.UINT, wintypes.WPARAM, wintypes.LPARAM)


class WNDCLASSEXW(ctypes.Structure):
    _fields_ = [("cbSize", wintypes.UINT), ("style", wintypes.UINT), ("lpfnWndProc", WNDPROC),
                ("cbClsExtra", ctypes.c_int), ("cbWndExtra", ctypes.c_int), ("hInstance", wintypes.HINSTANCE),
                ("hIcon", wintypes.HICON), ("hCursor", wintypes.HANDLE), ("hbrBackground", wintypes.HBRUSH),
                ("lpszMenuName", wintypes.LPCWSTR), ("lpszClassName", wintypes.LPCWSTR), ("hIconSm", wintypes.HICON)]


class NOTIFYICONDATAW(ctypes.Structure):
    _fields_ = [("cbSize", wintypes.DWORD), ("hWnd", wintypes.HWND), ("uID", wintypes.UINT),
                ("uFlags", wintypes.UINT), ("uCallbackMessage", wintypes.UINT), ("hIcon", wintypes.HICON),
                ("szTip", wintypes.WCHAR * 128), ("dwState", wintypes.DWORD), ("dwStateMask", wintypes.DWORD),
                ("szInfo", wintypes.WCHAR * 256), ("uVersion", wintypes.UINT), ("szInfoTitle", wintypes.WCHAR * 64),
                ("dwInfoFlags", wintypes.DWORD), ("guidItem", ctypes.c_byte * 16), ("hBalloonIcon", wintypes.HICON)]


def _api():
    u = ctypes.WinDLL("user32", use_last_error=True)
    u.DefWindowProcW.argtypes = [wintypes.HWND, wintypes.UINT, wintypes.WPARAM, wintypes.LPARAM]
    u.DefWindowProcW.restype = LRESULT
    u.CreateWindowExW.restype = wintypes.HWND
    u.CreateWindowExW.argtypes = [wintypes.DWORD, wintypes.LPCWSTR, wintypes.LPCWSTR, wintypes.DWORD, ctypes.c_int,
                                  ctypes.c_int, ctypes.c_int, ctypes.c_int, wintypes.HWND, wintypes.HMENU,
                                  wintypes.HINSTANCE, wintypes.LPVOID]
    u.RegisterClassExW.argtypes = [ctypes.POINTER(WNDCLASSEXW)]
    u.LoadImageW.restype = wintypes.HANDLE
    u.LoadImageW.argtypes = [wintypes.HINSTANCE, wintypes.LPCWSTR, wintypes.UINT, ctypes.c_int, ctypes.c_int, wintypes.UINT]
    u.CreatePopupMenu.restype = wintypes.HMENU
    u.AppendMenuW.argtypes = [wintypes.HMENU, wintypes.UINT, ctypes.c_size_t, wintypes.LPCWSTR]
    u.TrackPopupMenu.argtypes = [wintypes.HMENU, wintypes.UINT, ctypes.c_int, ctypes.c_int, ctypes.c_int,
                                 wintypes.HWND, wintypes.LPVOID]
    u.PostMessageW.argtypes = [wintypes.HWND, wintypes.UINT, wintypes.WPARAM, wintypes.LPARAM]
    u.RegisterWindowMessageW.restype = wintypes.UINT
    return u


# ---- icons -----------------------------------------------------------------------
def make_ico(color: tuple[int, int, int], size: int = 32) -> bytes:
    """A dark rounded square with a colored dot, as a 32-bit .ico."""
    ss = 4  # supersampling for smooth edges
    px = bytearray()
    for y in range(size - 1, -1, -1):  # bottom-up rows
        for x in range(size):
            bg = dot = 0
            for sy in range(ss):
                for sx in range(ss):
                    fx, fy = (x + (sx + .5) / ss) / size, (y + (sy + .5) / ss) / size
                    r = 0.22
                    cx, cy = min(max(fx, r), 1 - r), min(max(fy, r), 1 - r)
                    if (fx - cx) ** 2 + (fy - cy) ** 2 <= r * r:
                        bg += 1
                    if (fx - .5) ** 2 + (fy - .5) ** 2 <= 0.25 ** 2:
                        dot += 1
            a_bg, a_dot = bg / ss ** 2, dot / ss ** 2
            base = (36, 32, 31)  # BGR of #1f2024
            c = tuple(int(base[i] * (1 - a_dot) + color[2 - i] * a_dot) for i in range(3))
            px += bytes((*c, int(255 * max(a_bg, a_dot))))
    mask = bytes(((size + 31) // 32) * 4 * size)
    bmp = struct.pack("<IiiHHIIiiII", 40, size, size * 2, 1, 32, 0, len(px) + len(mask), 0, 0, 0, 0) + px + mask
    return struct.pack("<HHH", 0, 1, 1) + struct.pack("<BBBBHHII", size, size, 0, 0, 1, 32, len(bmp), 22) + bmp


ICON_COLORS = {"idle": (150, 155, 170), "watching": (34, 197, 94), "rec": (239, 68, 68), "clip": (245, 158, 11)}


class Tray:
    def __init__(self, icon_dir: Path, on_open: Callable[[], None], on_quit: Callable[[], None],
                 on_hotkey: Callable[[], None], menu_extra: Callable[[], list[tuple[str, Callable[[], None] | None]]] | None = None,
                 log=print) -> None:
        self.icon_dir = icon_dir
        self.on_open, self.on_quit, self.on_hotkey = on_open, on_quit, on_hotkey
        self.menu_extra = menu_extra or (lambda: [])
        self.log = log
        self.hwnd = None
        self._icons: dict[str, int] = {}
        self._state = ("idle", "OBSPlayed")
        self._hotkey: tuple[int, int] | None = None
        self._wanted_hotkey: tuple[int, int] | None = None
        self._menu_actions: dict[int, Callable[[], None]] = {}
        self._ready = threading.Event()

    def _icon(self, name: str) -> int:
        if name not in self._icons:
            self.icon_dir.mkdir(parents=True, exist_ok=True)
            path = self.icon_dir / f"tray-{name}.ico"
            path.write_bytes(make_ico(ICON_COLORS[name]))
            self._icons[name] = self.u.LoadImageW(None, str(path), 1, 0, 0, 0x10 | 0x40)  # IMAGE_ICON, LOADFROMFILE|DEFAULTSIZE
        return self._icons[name]

    def _nid(self) -> NOTIFYICONDATAW:
        nid = NOTIFYICONDATAW()
        nid.cbSize = ctypes.sizeof(nid)
        nid.hWnd = self.hwnd
        nid.uID = 1
        return nid

    def _apply_icon(self, msg: int = NIM_MODIFY) -> None:
        nid = self._nid()
        nid.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP
        nid.uCallbackMessage = WM_TRAY
        nid.hIcon = self._icon(self._state[0])
        nid.szTip = self._state[1][:127]
        ctypes.windll.shell32.Shell_NotifyIconW(msg, ctypes.byref(nid))

    # ---- thread-safe API ------------------------------------------------------------
    def set_state(self, icon: str, tip: str) -> None:
        if (icon, tip) != self._state:
            self._state = (icon, tip)
            if self.hwnd:
                self.u.PostMessageW(self.hwnd, WM_REFRESH, 0, 0)

    def set_hotkey(self, spec: str) -> None:
        self._wanted_hotkey = parse_hotkey(spec)
        if self.hwnd:
            self.u.PostMessageW(self.hwnd, WM_REFRESH, 0, 0)

    def notify(self, title: str, text: str) -> None:
        if not self.hwnd:
            return
        nid = self._nid()
        nid.uFlags = NIF_INFO
        nid.szInfoTitle = title[:63]
        nid.szInfo = text[:255]
        ctypes.windll.shell32.Shell_NotifyIconW(NIM_MODIFY, ctypes.byref(nid))

    def quit(self) -> None:
        if self.hwnd:
            self.u.PostMessageW(self.hwnd, WM_QUIT_APP, 0, 0)

    # ---- window thread -----------------------------------------------------------------
    def _sync_hotkey(self) -> None:
        if self._wanted_hotkey == self._hotkey:
            return
        if self._hotkey:
            self.u.UnregisterHotKey(self.hwnd, HOTKEY_ID)
            self._hotkey = None
        if self._wanted_hotkey:
            mods, vk = self._wanted_hotkey
            if self.u.RegisterHotKey(self.hwnd, HOTKEY_ID, mods | MOD_NOREPEAT, vk):
                self._hotkey = self._wanted_hotkey
            else:
                self.log("[tray] the highlight hotkey is already used by another app")
                self._wanted_hotkey = None

    def _show_menu(self) -> None:
        u = self.u
        menu = u.CreatePopupMenu()
        self._menu_actions = {}
        items = [("Open OBSPlayed", self.on_open)] + self.menu_extra() + [("-", None), ("Quit OBSPlayed", self.on_quit)]
        for i, (label, action) in enumerate(items, start=100):
            if label == "-":
                u.AppendMenuW(menu, 0x800, 0, None)  # MF_SEPARATOR
            else:
                u.AppendMenuW(menu, 0 if action else 0x1, i, label)  # MF_STRING / MF_GRAYED
                if action:
                    self._menu_actions[i] = action
        pt = wintypes.POINT()
        u.GetCursorPos(ctypes.byref(pt))
        u.SetForegroundWindow(self.hwnd)
        cmd = u.TrackPopupMenu(menu, 0x0100 | 0x0002, pt.x, pt.y, 0, self.hwnd, None)  # RETURNCMD | RIGHTBUTTON
        u.PostMessageW(self.hwnd, 0, 0, 0)
        u.DestroyMenu(menu)
        action = self._menu_actions.get(cmd)
        if action:
            threading.Thread(target=action, daemon=True).start()

    def run(self) -> None:
        """Create the icon and run the message loop (blocks until quit)."""
        u = self.u = _api()
        taskbar_created = u.RegisterWindowMessageW("TaskbarCreated")

        def proc(hwnd, msg, wparam, lparam):
            try:
                if msg == WM_TRAY:
                    if lparam in (WM_LBUTTONUP, WM_LBUTTONDBLCLK):
                        threading.Thread(target=self.on_open, daemon=True).start()
                    elif lparam == WM_RBUTTONUP:
                        self._show_menu()
                    return 0
                if msg == WM_HOTKEY and wparam == HOTKEY_ID:
                    threading.Thread(target=self.on_hotkey, daemon=True).start()
                    return 0
                if msg == WM_REFRESH:
                    self._apply_icon()
                    self._sync_hotkey()
                    return 0
                if msg == taskbar_created:  # Explorer restarted
                    self._apply_icon(NIM_ADD)
                    return 0
                if msg == WM_QUIT_APP:
                    u.DestroyWindow(hwnd)
                    return 0
                if msg == WM_DESTROY:
                    ctypes.windll.shell32.Shell_NotifyIconW(NIM_DELETE, ctypes.byref(self._nid()))
                    u.PostQuitMessage(0)
                    return 0
            except Exception as exc:  # never let an exception escape into Windows
                self.log(f"[tray] {exc!r}")
            return u.DefWindowProcW(hwnd, msg, wparam, lparam)

        self._proc = WNDPROC(proc)  # keep a reference
        hinst = ctypes.windll.kernel32.GetModuleHandleW(None)
        wc = WNDCLASSEXW()
        wc.cbSize = ctypes.sizeof(wc)
        wc.lpfnWndProc = self._proc
        wc.hInstance = hinst
        wc.lpszClassName = "OBSPlayedTray"
        u.RegisterClassExW(ctypes.byref(wc))
        self.hwnd = u.CreateWindowExW(0, "OBSPlayedTray", "OBSPlayed", 0, 0, 0, 0, 0, None, None, hinst, None)
        self._apply_icon(NIM_ADD)
        self._sync_hotkey()
        self._ready.set()
        msg = wintypes.MSG()
        while u.GetMessageW(ctypes.byref(msg), None, 0, 0) > 0:
            u.TranslateMessage(ctypes.byref(msg))
            u.DispatchMessageW(ctypes.byref(msg))
        self.hwnd = None


VK = {f"F{i}": 0x6F + i for i in range(1, 13)}


def parse_hotkey(spec: str) -> tuple[int, int] | None:
    """"Ctrl+Shift+F8" -> (modifiers, virtual key); "off"/"" -> None."""
    if not spec or spec.lower() == "off":
        return None
    mods, vk = 0, None
    for part in spec.split("+"):
        p = part.strip().lower()
        if p in ("ctrl", "control"):
            mods |= MOD_CONTROL
        elif p == "alt":
            mods |= MOD_ALT
        elif p == "shift":
            mods |= MOD_SHIFT
        elif p.upper() in VK:
            vk = VK[p.upper()]
        elif len(p) == 1 and p.isalnum():
            vk = ord(p.upper())
    return (mods, vk) if vk else None
