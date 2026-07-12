// Std-handle birth probe for issue #450.
//
// Spawned as a pane command by test_issue450_stdhandle_corruption.ps1.
// Under ConPTY a healthy child's std handles are console character devices:
// GetFileType == FILE_TYPE_CHAR (2) and GetConsoleMode succeeds.  A child
// born during an unsynchronized FreeConsole/AttachConsole dance instead
// inherits freed, recycled handle values (GetFileType 0, ERROR_INVALID_HANDLE),
// which is what killed pwsh with FailFast in the original reports.
using System;
using System.IO;
using System.Runtime.InteropServices;

class HandleCheck {
    [DllImport("kernel32.dll", SetLastError = true)]
    static extern IntPtr GetStdHandle(int nStdHandle);
    [DllImport("kernel32.dll", SetLastError = true)]
    static extern uint GetFileType(IntPtr hFile);
    [DllImport("kernel32.dll", SetLastError = true)]
    static extern bool GetConsoleMode(IntPtr hConsoleHandle, out uint lpMode);

    const int STD_INPUT_HANDLE = -10;
    const int STD_OUTPUT_HANDLE = -11;
    const int STD_ERROR_HANDLE = -12;

    static string Probe(string name, int which) {
        IntPtr h = GetStdHandle(which);
        uint ft = GetFileType(h);
        int ftErr = Marshal.GetLastWin32Error();
        uint mode;
        bool cm = GetConsoleMode(h, out mode);
        int cmErr = Marshal.GetLastWin32Error();
        bool ok = (ft == 2) && cm;
        return string.Format("{0}: h=0x{1:X} ftype={2}(e{3}) conmode={4}(e{5}) => {6}",
            name, h.ToInt64(), ft, ftErr, cm ? "OK" : "FAIL", cmErr, ok ? "GOOD" : "CORRUPT");
    }

    static void Main(string[] args) {
        string logPath = args.Length > 0 ? args[0] : Path.Combine(Path.GetTempPath(), "psmux450_handlecheck.log");
        string line;
        try {
            string a = Probe("IN", STD_INPUT_HANDLE);
            string b = Probe("OUT", STD_OUTPUT_HANDLE);
            string c = Probe("ERR", STD_ERROR_HANDLE);
            bool anyCorrupt = a.Contains("CORRUPT") || b.Contains("CORRUPT") || c.Contains("CORRUPT");
            line = string.Format("PID={0} VERDICT={1} | {2} | {3} | {4}",
                System.Diagnostics.Process.GetCurrentProcess().Id,
                anyCorrupt ? "CORRUPT" : "GOOD", a, b, c);
        } catch (Exception ex) {
            line = "EXCEPTION " + ex.Message;
        }
        for (int i = 0; i < 10; i++) {
            try {
                using (var sw = new StreamWriter(logPath, true)) { sw.WriteLine(line); }
                break;
            } catch { System.Threading.Thread.Sleep(50); }
        }
        // Stay alive briefly so the pane isn't torn down mid-write.
        System.Threading.Thread.Sleep(1500);
    }
}
