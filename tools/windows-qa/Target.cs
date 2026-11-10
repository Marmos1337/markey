// A small window to type into during Windows QA. It is its own executable because
// MarKey leaves powershell.exe alone. It reports what its fields hold in a state
// file and takes commands from a second file.
using System;
using System.Drawing;
using System.Globalization;
using System.IO;
using System.Text;
using System.Windows.Forms;

static class Program
{
    [STAThread]
    static void Main(string[] args)
    {
        Application.EnableVisualStyles();
        Application.Run(new TargetForm(args[0]));
    }
}

sealed class TargetForm : Form
{
    static bool slowPaste;
    readonly string state;
    readonly TextBox normal = new TextBox();
    readonly TextBox password = new TextBox();
    readonly Timer timer = new Timer();

    public TargetForm(string statePath)
    {
        state = statePath;
        Text = "QA Target";
        StartPosition = FormStartPosition.Manual;
        Location = new Point(40, 40);
        ClientSize = new Size(640, 120);
        TopMost = true;
        normal.SetBounds(20, 20, 600, 28);
        password.SetBounds(20, 70, 600, 28);
        password.UseSystemPasswordChar = true;
        Controls.Add(normal);
        Controls.Add(password);
        normal.KeyDown += SelectAll;
        password.KeyDown += SelectAll;
        timer.Interval = 80;
        timer.Tick += delegate { Commands(); Write(); };
        timer.Start();
    }

    // A single-line TextBox does not take Ctrl+A by itself.
    static void SelectAll(object sender, KeyEventArgs e)
    {
        // An application too busy to read the clipboard straight away.
        if (slowPaste && e.Control && e.KeyCode == Keys.V) System.Threading.Thread.Sleep(700);
        if (e.Control && e.KeyCode == Keys.A)
        {
            ((TextBox)sender).SelectAll();
            e.SuppressKeyPress = true;
        }
    }

    void Commands()
    {
        string file = state + ".cmd";
        if (!File.Exists(file)) return;
        string text;
        try { text = File.ReadAllText(file); File.Delete(file); }
        catch (IOException) { return; }
        foreach (string raw in text.Split('\n'))
        {
            string c = raw.Trim();
            if (c == "clear") { normal.Text = ""; password.Text = ""; }
            else if (c == "focus:normal") { Activate(); normal.Focus(); }
            else if (c == "focus:password") { Activate(); password.Focus(); }
            else if (c == "layout:en") SwitchLayout("en-US");
            else if (c == "layout:ru") SwitchLayout("ru-RU");
            else if (c == "slow:on") slowPaste = true;
            else if (c == "slow:off") slowPaste = false;
        }
    }

    static void SwitchLayout(string culture)
    {
        foreach (InputLanguage l in InputLanguage.InstalledInputLanguages)
            if (l.Culture.Name == culture) InputLanguage.CurrentInputLanguage = l;
    }

    static string B64(string s) { return Convert.ToBase64String(Encoding.UTF8.GetBytes(s)); }

    void Write()
    {
        Rectangle r = normal.RectangleToScreen(normal.ClientRectangle);
        var b = new StringBuilder();
        b.Append("normal=").Append(B64(normal.Text)).Append('\n');
        b.Append("password=").Append(B64(password.Text)).Append('\n');
        b.Append("focus=").Append(normal.Focused ? "normal" : password.Focused ? "password" : "other").Append('\n');
        b.Append("layout=").Append(InputLanguage.CurrentInputLanguage.Culture.Name).Append('\n');
        b.Append("caps=").Append(IsKeyLocked(Keys.CapsLock) ? "1" : "0").Append('\n');
        b.Append("rect=").Append(r.X + "," + r.Y + "," + r.Width + "," + r.Height).Append('\n');
        b.Append("hwnd=").Append(Handle.ToInt64().ToString(CultureInfo.InvariantCulture)).Append('\n');
        string tmp = state + ".tmp";
        try
        {
            File.WriteAllText(tmp, b.ToString());
            File.Copy(tmp, state, true);
        }
        catch (IOException) { }
    }
}
