#!/usr/bin/env python3
"""Drive a real GPUI HWND using Win32 input; UI state is read-only telemetry."""

import ctypes as c
from ctypes import wintypes as w
import os
from pathlib import Path
import time

from ui_driver import Driver

if os.name != 'nt':
    raise RuntimeError('The Windows UI driver requires Windows')

u = c.WinDLL('user32', use_last_error=True)
k = c.WinDLL('kernel32', use_last_error=True)
u.SetProcessDpiAwarenessContext(c.c_void_p(-4))
ULONG_PTR = c.c_size_t


class MouseInput(c.Structure):
    _fields_ = [('dx', w.LONG), ('dy', w.LONG), ('mouseData', w.DWORD),
                ('dwFlags', w.DWORD), ('time', w.DWORD), ('dwExtraInfo', ULONG_PTR)]


class KeyboardInput(c.Structure):
    _fields_ = [('wVk', w.WORD), ('wScan', w.WORD), ('dwFlags', w.DWORD),
                ('time', w.DWORD), ('dwExtraInfo', ULONG_PTR)]


class HardwareInput(c.Structure):
    _fields_ = [('uMsg', w.DWORD), ('wParamL', w.WORD), ('wParamH', w.WORD)]


class InputUnion(c.Union):
    _fields_ = [('mi', MouseInput), ('ki', KeyboardInput), ('hi', HardwareInput)]


class Input(c.Structure):
    _anonymous_ = ('value',)
    _fields_ = [('type', w.DWORD), ('value', InputUnion)]


WNDENUMPROC = c.WINFUNCTYPE(w.BOOL, w.HWND, w.LPARAM)
u.EnumWindows.argtypes = [WNDENUMPROC, w.LPARAM]
u.GetWindowThreadProcessId.argtypes = [w.HWND, c.POINTER(w.DWORD)]
u.GetForegroundWindow.restype = w.HWND
u.SetForegroundWindow.argtypes = [w.HWND]
u.ShowWindow.argtypes = [w.HWND, c.c_int]
u.IsWindowVisible.argtypes = [w.HWND]
u.IsIconic.argtypes = [w.HWND]
u.IsZoomed.argtypes = [w.HWND]
u.GetWindowRect.argtypes = [w.HWND, c.POINTER(w.RECT)]
u.GetClientRect.argtypes = [w.HWND, c.POINTER(w.RECT)]
u.ClientToScreen.argtypes = [w.HWND, c.POINTER(w.POINT)]
u.GetDpiForWindow.argtypes = [w.HWND]
u.SetWindowPos.argtypes = [w.HWND, w.HWND, c.c_int, c.c_int, c.c_int, c.c_int, w.UINT]
u.SendInput.argtypes = [w.UINT, c.POINTER(Input), c.c_int]
u.PostMessageW.argtypes = [w.HWND, w.UINT, w.WPARAM, w.LPARAM]
u.GetClassNameW.argtypes = [w.HWND, w.LPWSTR, c.c_int]
u.GetWindowTextW.argtypes = [w.HWND, w.LPWSTR, c.c_int]
u.AttachThreadInput.argtypes = [w.DWORD, w.DWORD, w.BOOL]
u.BringWindowToTop.argtypes = [w.HWND]
u.SetFocus.argtypes = [w.HWND]
u.SendMessageW.argtypes = [w.HWND, w.UINT, w.WPARAM, w.LPARAM]
u.SendMessageW.restype = c.c_ssize_t
u.GetMenuItemCount.argtypes = [w.HMENU]
u.GetMenuStringW.argtypes = [w.HMENU, w.UINT, w.LPWSTR, c.c_int, w.UINT]
u.GetMenuItemRect.argtypes = [w.HWND, w.HMENU, w.UINT, c.POINTER(w.RECT)]
u.OpenClipboard.argtypes = [w.HWND]
u.GetClipboardData.argtypes = [w.UINT]
u.GetClipboardData.restype = w.HANDLE
k.GlobalLock.argtypes = [w.HGLOBAL]
k.GlobalLock.restype = c.c_void_p
k.GlobalUnlock.argtypes = [w.HGLOBAL]


def windows(pid=None, class_name=None):
    found = []

    @WNDENUMPROC
    def visit(hwnd, _):
        owner = w.DWORD()
        u.GetWindowThreadProcessId(hwnd, c.byref(owner))
        name = c.create_unicode_buffer(256)
        u.GetClassNameW(hwnd, name, len(name))
        if (u.IsWindowVisible(hwnd) and (pid is None or owner.value == pid)
                and (class_name is None or name.value == class_name)):
            found.append(hwnd)
        return True

    u.EnumWindows(visit, 0)
    return found


def title(hwnd):
    value = c.create_unicode_buffer(256)
    u.GetWindowTextW(hwnd, value, len(value))
    return value.value


class WindowsDriver(Driver):
    def __init__(self, pid, state_path):
        super().__init__('', state_path)
        self.pid = pid
        deadline = time.monotonic() + 45
        while time.monotonic() < deadline:
            handles = [h for h in windows(pid) if title(h) == 'Everything']
            if handles and Path(state_path).exists():
                self.hwnd = handles[0]
                self.activate()
                return
            time.sleep(0.05)
        raise TimeoutError(f'GPUI window did not appear for PID {pid}')

    def activate(self):
        u.ShowWindow(self.hwnd, 9 if u.IsIconic(self.hwnd) else 5)
        u.SetForegroundWindow(self.hwnd)
        time.sleep(0.05)
        if u.GetForegroundWindow() != self.hwnd:
            current = k.GetCurrentThreadId()
            foreground = u.GetWindowThreadProcessId(u.GetForegroundWindow(), None)
            attached = current != foreground and u.AttachThreadInput(current, foreground, True)
            try:
                u.BringWindowToTop(self.hwnd)
                u.SetForegroundWindow(self.hwnd)
                u.SetFocus(self.hwnd)
            finally:
                if attached:
                    u.AttachThreadInput(current, foreground, False)
            time.sleep(0.05)
        if u.GetForegroundWindow() != self.hwnd:
            raise RuntimeError('Cannot focus the test HWND; refusing global input')

    def inputs(self, events):
        if u.GetForegroundWindow() != self.hwnd:
            self.activate()
        values = (Input * len(events))(*events)
        if u.SendInput(len(events), values, c.sizeof(Input)) != len(events):
            raise c.WinError(c.get_last_error())
        time.sleep(0.035)

    def keyboard(self, vk, up=False, scan=0, flags=0):
        return Input(type=1, ki=KeyboardInput(vk, scan, flags | (2 if up else 0), 0, 0))

    def key(self, combination):
        parts = combination.split('-')
        name = parts[-1]
        keys = dict(enter=13, escape=27, backspace=8, delete=46, down=40,
                    up=38, left=37, right=39, pagedown=34, pageup=33,
                    f2=113, space=32, tab=9)
        vk = keys.get(name, ord(name.upper()) if len(name) == 1 else None)
        if vk is None:
            raise ValueError(f'Unsupported key: {combination}')
        modifiers = [dict(ctrl=17, cmd=17, shift=16, alt=18)[p] for p in parts[:-1]]
        extended = 1 if name in ['delete', 'down', 'up', 'left', 'right', 'pagedown', 'pageup'] else 0
        self.inputs([*(self.keyboard(v) for v in modifiers),
                     self.keyboard(vk, flags=extended), self.keyboard(vk, up=True, flags=extended),
                     *(self.keyboard(v, up=True) for v in reversed(modifiers))])

    def text(self, text):
        events = []
        encoded = text.encode('utf-16-le')
        for i in range(0, len(encoded), 2):
            unit = int.from_bytes(encoded[i:i+2], 'little')
            events.extend([self.keyboard(0, scan=unit, flags=4),
                           self.keyboard(0, up=True, scan=unit, flags=4)])
        if events:
            self.inputs(events)

    def point(self, x, y):
        scale = u.GetDpiForWindow(self.hwnd) / 96
        pt = w.POINT(round(x * scale), round(y * scale))
        if not u.ClientToScreen(self.hwnd, c.byref(pt)):
            raise c.WinError(c.get_last_error())
        return pt

    def mouse(self, op, x, y, button='left', modifiers=(), count=1, **_):
        self.activate()
        pt = self.point(x, y)
        u.SetCursorPos(pt.x, pt.y)
        down, up = (8, 16) if button == 'right' else (2, 4)
        flags = dict(click=[down, up] * count, down=[down], up=[up], move=[1])[op]
        mods = [dict(ctrl=17, cmd=17, shift=16, alt=18)[m] for m in modifiers]
        self.inputs([*(self.keyboard(v) for v in mods),
                     *(Input(type=0, mi=MouseInput(0, 0, 0, f, 0, 0)) for f in flags),
                     *(self.keyboard(v, up=True) for v in reversed(mods))])

    def capture(self, path):
        from PIL import ImageGrab
        self.activate()
        time.sleep(0.15)
        rect = w.RECT()
        u.GetWindowRect(self.hwnd, c.byref(rect))
        ImageGrab.grab(bbox=(rect.left, rect.top, rect.right, rect.bottom),
                       all_screens=True).save(path)

    def resize(self, width, height):
        rect = w.RECT()
        u.GetWindowRect(self.hwnd, c.byref(rect))
        client = w.RECT()
        u.GetClientRect(self.hwnd, c.byref(client))
        scale = u.GetDpiForWindow(self.hwnd) / 96
        outer_w = round(width * scale) + rect.right - rect.left - client.right
        outer_h = round(height * scale) + rect.bottom - rect.top - client.bottom
        if not u.SetWindowPos(self.hwnd, None, 0, 0, outer_w, outer_h, 0x16):
            raise c.WinError(c.get_last_error())
        time.sleep(0.1)

    def clipboard(self):
        for _ in range(20):
            if u.OpenClipboard(self.hwnd):
                break
            time.sleep(0.025)
        else:
            raise c.WinError(c.get_last_error())
        try:
            handle = u.GetClipboardData(13)  # CF_UNICODETEXT
            pointer = k.GlobalLock(handle) if handle else None
            if not pointer:
                return {'text': ''}
            try:
                return {'text': c.wstring_at(pointer)}
            finally:
                k.GlobalUnlock(handle)
        finally:
            u.CloseClipboard()

    def menu(self):
        for hwnd in windows(self.pid, '#32768'):
            menu = u.SendMessageW(hwnd, 0x01E1, 0, 0)  # MN_GETHMENU
            items = []
            for i in range(u.GetMenuItemCount(menu)):
                label = c.create_unicode_buffer(512)
                u.GetMenuStringW(menu, i, label, len(label), 0x400)
                items.append(label.value)
            return hwnd, menu, items
        raise RuntimeError('No native popup menu belonging to the test process')

    def choose_menu(self, label):
        _, menu, items = self.menu()
        index = items.index(label)
        rect = w.RECT()
        if not u.GetMenuItemRect(self.hwnd, menu, index, c.byref(rect)):
            raise c.WinError(c.get_last_error())
        u.SetCursorPos((rect.left + rect.right)//2, (rect.top + rect.bottom)//2)
        self.inputs([Input(type=0, mi=MouseInput(0, 0, 0, f, 0, 0)) for f in [2, 4]])

    def send(self, op, **values):
        if op in ['click', 'down', 'up', 'move']:
            return self.mouse(op, **values)
        if op == 'capture':
            return self.capture(values['path'])
        if op == 'resize':
            return self.resize(values['width'], values['height'])
        if op == 'clipboard':
            return self.clipboard()
        if op == 'key':
            return self.key(values['key'])
        if op == 'text':
            return self.text(values['text'])
        raise ValueError(f'Unsupported Windows driver operation: {op}')
