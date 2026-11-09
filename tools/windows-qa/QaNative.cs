// What PowerShell cannot do by itself on the VM: list windows, measure whether a
// window painted, and press keys and the mouse through SendInput, which is what
// MarKey's low-level hooks see.
using System;
using System.Collections.Generic;
using System.Drawing;
using System.Drawing.Imaging;
using System.Runtime.InteropServices;
using System.Text;
using System.Threading;

public static class QaNative
{
    public sealed class Win
    {
        public long Hwnd; public int Pid; public string Title; public string ClassName;
        public bool Visible; public int X, Y, Width, Height;
    }

    public sealed class Paint { public int Width, Height, Distinct; public double Variance; }

    [StructLayout(LayoutKind.Sequential)] struct RECT { public int Left, Top, Right, Bottom; }
    [StructLayout(LayoutKind.Sequential)] struct POINT { public int X, Y; }
    [StructLayout(LayoutKind.Sequential)] struct KEYBDINPUT { public ushort wVk, wScan; public uint dwFlags, time; public IntPtr dwExtraInfo; }
    [StructLayout(LayoutKind.Sequential)] struct MOUSEINPUT { public int dx, dy; public uint mouseData, dwFlags, time; public IntPtr dwExtraInfo; }
    [StructLayout(LayoutKind.Explicit)] struct UNION { [FieldOffset(0)] public KEYBDINPUT ki; [FieldOffset(0)] public MOUSEINPUT mi; }
    [StructLayout(LayoutKind.Sequential)] struct INPUT { public uint type; public UNION u; }

    delegate bool EnumProc(IntPtr hwnd, IntPtr param);
    [DllImport("user32.dll")] static extern bool EnumWindows(EnumProc proc, IntPtr param);
    [DllImport("user32.dll")] static extern uint GetWindowThreadProcessId(IntPtr hwnd, out uint pid);
    [DllImport("user32.dll")] static extern bool IsWindowVisible(IntPtr hwnd);
    [DllImport("user32.dll", CharSet = CharSet.Unicode)] static extern int GetWindowTextW(IntPtr hwnd, StringBuilder text, int max);
    [DllImport("user32.dll", CharSet = CharSet.Unicode)] static extern int GetClassNameW(IntPtr hwnd, StringBuilder text, int max);
    [DllImport("user32.dll")] static extern bool GetWindowRect(IntPtr hwnd, out RECT rect);
    [DllImport("user32.dll")] static extern bool ClientToScreen(IntPtr hwnd, ref POINT point);
    [DllImport("user32.dll")] static extern bool PrintWindow(IntPtr hwnd, IntPtr hdc, uint flags);
    [DllImport("user32.dll")] static extern bool PostMessageW(IntPtr hwnd, uint message, IntPtr wParam, IntPtr lParam);
    [DllImport("user32.dll")] static extern bool SetForegroundWindow(IntPtr hwnd);
    [DllImport("user32.dll")] static extern IntPtr GetForegroundWindow();
    [DllImport("user32.dll")] static extern bool SetCursorPos(int x, int y);
    [DllImport("user32.dll")] static extern uint SendInput(uint count, INPUT[] inputs, int size);
    [DllImport("user32.dll")] static extern uint MapVirtualKeyW(uint code, uint mapType);
    [DllImport("user32.dll", CharSet = CharSet.Unicode)] static extern IntPtr LoadKeyboardLayoutW(string id, uint flags);
    [DllImport("user32.dll")] static extern IntPtr GetKeyboardLayout(uint thread);
    [DllImport("user32.dll")] static extern int GetKeyboardLayoutList(int count, IntPtr[] list);
    [DllImport("user32.dll")] static extern IntPtr WindowFromPoint(POINT point);
    [DllImport("user32.dll")] static extern IntPtr GetAncestor(IntPtr hwnd, uint flags);

    const uint KEYUP = 2, EXTENDED = 1, MOUSE_DOWN = 2, MOUSE_UP = 4, WM_CLOSE = 0x10, WM_INPUTLANGCHANGEREQUEST = 0x50, GA_ROOT = 2;

    /// <summary>On a computer someone uses: the one window input may go to. Zero means anywhere.</summary>
    public static long Guard;

    static void RequireGuard()
    {
        if (Guard != 0 && GetForegroundWindow().ToInt64() != Guard)
            throw new InvalidOperationException("The test window lost the foreground; the key was not sent.");
    }

    public static List<Win> Windows(int pid)
    {
        var list = new List<Win>();
        EnumProc visit = delegate (IntPtr hwnd, IntPtr p)
        {
            uint owner;
            GetWindowThreadProcessId(hwnd, out owner);
            if ((int)owner != pid) return true;
            var title = new StringBuilder(512);
            var cls = new StringBuilder(256);
            GetWindowTextW(hwnd, title, title.Capacity);
            GetClassNameW(hwnd, cls, cls.Capacity);
            RECT r;
            GetWindowRect(hwnd, out r);
            list.Add(new Win { Hwnd = hwnd.ToInt64(), Pid = pid, Title = title.ToString(), ClassName = cls.ToString(),
                Visible = IsWindowVisible(hwnd), X = r.Left, Y = r.Top, Width = r.Right - r.Left, Height = r.Bottom - r.Top });
            return true;
        };
        EnumWindows(visit, IntPtr.Zero);
        GC.KeepAlive(visit);
        return list;
    }

    public static long Foreground() { return GetForegroundWindow().ToInt64(); }
    public static bool Front(long hwnd) { return SetForegroundWindow(new IntPtr(hwnd)); }
    public static void Close(long hwnd) { PostMessageW(new IntPtr(hwnd), WM_CLOSE, IntPtr.Zero, IntPtr.Zero); }
    public static int[] ClientOrigin(long hwnd)
    {
        var p = new POINT();
        ClientToScreen(new IntPtr(hwnd), ref p);
        return new[] { p.X, p.Y };
    }

    public static bool LoadRussian() { return LoadKeyboardLayoutW("00000419", 0) != IntPtr.Zero; }

    public static long ForegroundLayout()
    {
        uint pid;
        return GetKeyboardLayout(GetWindowThreadProcessId(GetForegroundWindow(), out pid)).ToInt64();
    }

    public static bool SetForegroundLayout(long hkl)
    {
        IntPtr hwnd = GetForegroundWindow();
        return hwnd != IntPtr.Zero && PostMessageW(hwnd, WM_INPUTLANGCHANGEREQUEST, IntPtr.Zero, new IntPtr(hkl));
    }

    public static long[] Layouts()
    {
        var list = new IntPtr[32];
        int count = GetKeyboardLayoutList(list.Length, list);
        var result = new long[count];
        for (int i = 0; i < count; i++) result[i] = list[i].ToInt64();
        return result;
    }

    static Paint Measure(Bitmap bmp)
    {
        var seen = new HashSet<int>();
        double sum = 0, sq = 0; long n = 0;
        for (int y = 0; y < bmp.Height; y += 3)
            for (int x = 0; x < bmp.Width; x += 3)
            {
                Color c = bmp.GetPixel(x, y);
                seen.Add(c.ToArgb());
                double v = 0.3 * c.R + 0.59 * c.G + 0.11 * c.B;
                sum += v; sq += v * v; n++;
            }
        double mean = n == 0 ? 0 : sum / n;
        return new Paint { Width = bmp.Width, Height = bmp.Height, Distinct = seen.Count, Variance = n == 0 ? 0 : sq / n - mean * mean };
    }

    /// <summary>PrintWindow with full content, saved as a PNG.</summary>
    public static Paint ShotWindow(long hwnd, string path)
    {
        RECT r;
        GetWindowRect(new IntPtr(hwnd), out r);
        using (var bmp = new Bitmap(Math.Max(1, r.Right - r.Left), Math.Max(1, r.Bottom - r.Top), PixelFormat.Format32bppArgb))
        {
            using (var g = Graphics.FromImage(bmp))
            {
                IntPtr dc = g.GetHdc();
                PrintWindow(new IntPtr(hwnd), dc, 2);
                g.ReleaseHdc(dc);
            }
            bmp.Save(path, ImageFormat.Png);
            return Measure(bmp);
        }
    }

    /// <summary>The whole primary screen, saved as a PNG.</summary>
    public static Paint ShotScreen(string path)
    {
        var size = System.Windows.Forms.Screen.PrimaryScreen.Bounds.Size;
        using (var bmp = new Bitmap(size.Width, size.Height, PixelFormat.Format32bppArgb))
        {
            using (var g = Graphics.FromImage(bmp)) g.CopyFromScreen(0, 0, 0, 0, size);
            bmp.Save(path, ImageFormat.Png);
            return Measure(bmp);
        }
    }

    static INPUT KeyInput(ushort vk, bool down, bool extended)
    {
        var i = new INPUT { type = 1 };
        i.u.ki.wVk = vk;
        i.u.ki.wScan = (ushort)MapVirtualKeyW(vk, 0);
        i.u.ki.dwFlags = (down ? 0 : KEYUP) | (extended ? EXTENDED : 0);
        return i;
    }

    static uint Send(params INPUT[] inputs) { return SendInput((uint)inputs.Length, inputs, Marshal.SizeOf(typeof(INPUT))); }

    public static uint Key(int vk, bool down, bool extended)
    {
        if (down) RequireGuard();
        return Send(KeyInput((ushort)vk, down, extended));
    }

    /// <summary>Lets go of Shift, Ctrl and Alt, so a stopped test leaves no key held.</summary>
    public static void ReleaseModifiers()
    {
        foreach (int vk in new[] { 0xA0, 0xA1, 0xA2, 0xA4, 0xA5 }) Send(KeyInput((ushort)vk, false, false));
        Send(KeyInput(0xA3, false, true));
    }

    public static void Tap(int vk, bool extended)
    {
        Key(vk, true, extended);
        Thread.Sleep(30);
        Key(vk, false, extended);
    }

    /// <summary>Physical keys for a-z and space; capitals hold Shift. As fast as a quick typist.</summary>
    public static void Type(string keys)
    {
        foreach (char c in keys)
        {
            bool upper = char.IsUpper(c);
            int vk = c == ' ' ? 0x20 : char.ToUpperInvariant(c);
            if (upper) Key(0xA0, true, false);
            Tap(vk, false);
            if (upper) Key(0xA0, false, false);
            Thread.Sleep(60);
        }
    }

    public static void Click(int x, int y)
    {
        if (Guard != 0)
        {
            var point = new POINT { X = x, Y = y };
            if (GetAncestor(WindowFromPoint(point), GA_ROOT).ToInt64() != Guard)
                throw new InvalidOperationException("Another window covers the test window; the click was not sent.");
        }
        SetCursorPos(x, y);
        Thread.Sleep(60);
        var down = new INPUT { type = 0 };
        down.u.mi.dwFlags = MOUSE_DOWN;
        var up = new INPUT { type = 0 };
        up.u.mi.dwFlags = MOUSE_UP;
        Send(down);
        Thread.Sleep(40);
        Send(up);
    }
}
