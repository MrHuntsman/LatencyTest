# Synthesizes N left mouse clicks at the screen center via SendInput.
# Usage: powershell -File sendclicks.ps1 [-Count 5] [-IntervalMs 400]
param(
    [int]$Count = 5,
    [int]$IntervalMs = 400
)

$signature = @'
using System;
using System.Runtime.InteropServices;

public static class Clicker {
    [StructLayout(LayoutKind.Sequential)]
    public struct MOUSEINPUT {
        public int dx;
        public int dy;
        public uint mouseData;
        public uint dwFlags;
        public uint time;
        public IntPtr dwExtraInfo;
    }

    [StructLayout(LayoutKind.Explicit)]
    public struct INPUTUNION {
        [FieldOffset(0)] public MOUSEINPUT mi;
    }

    [StructLayout(LayoutKind.Sequential)]
    public struct INPUT {
        public uint type;
        public INPUTUNION u;
    }

    [DllImport("user32.dll", SetLastError = true)]
    public static extern uint SendInput(uint nInputs, INPUT[] pInputs, int cbSize);

    [DllImport("user32.dll")]
    public static extern bool SetCursorPos(int x, int y);

    public const uint INPUT_MOUSE = 0;
    public const uint MOUSEEVENTF_LEFTDOWN = 0x0002;
    public const uint MOUSEEVENTF_LEFTUP = 0x0004;

    public static void Click() {
        var down = new INPUT { type = INPUT_MOUSE };
        down.u.mi.dwFlags = MOUSEEVENTF_LEFTDOWN;
        var up = new INPUT { type = INPUT_MOUSE };
        up.u.mi.dwFlags = MOUSEEVENTF_LEFTUP;
        var inputs = new INPUT[] { down, up };
        uint sent = SendInput(2, inputs, Marshal.SizeOf(typeof(INPUT)));
        if (sent != 2) throw new Exception("SendInput sent " + sent);
    }
}
'@

Add-Type -TypeDefinition $signature -Language CSharp

# Screen center (primary monitor)
$w = [System.Windows.Forms.Screen]::PrimaryScreen.Bounds.Width
$h = [System.Windows.Forms.Screen]::PrimaryScreen.Bounds.Height
Add-Type -AssemblyName System.Windows.Forms
$w = [System.Windows.Forms.Screen]::PrimaryScreen.Bounds.Width
$h = [System.Windows.Forms.Screen]::PrimaryScreen.Bounds.Height
[Clicker]::SetCursorPos([int]($w/2), [int]($h/2)) | Out-Null

for ($i = 0; $i -lt $Count; $i++) {
    [Clicker]::Click()
    Start-Sleep -Milliseconds $IntervalMs
}
Write-Host "sent $Count clicks"
