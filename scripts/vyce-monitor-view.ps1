# The session panel's C#, second part: the window. See vyce-monitor-model.ps1.
$VyceMonitorView = @'
namespace Vyce.Monitor
{
    using System;
    using System.Collections.Generic;
    using System.ComponentModel;
    using System.Drawing;
    using System.Drawing.Drawing2D;
    using System.Runtime.InteropServices;
    using System.Windows.Forms;

    static class Native
    {
        [DllImport("user32.dll")]
        public static extern bool SetProcessDPIAware();
        [DllImport("dwmapi.dll")]
        public static extern int DwmSetWindowAttribute(IntPtr window, int attribute, ref int value, int size);
        [DllImport("uxtheme.dll", CharSet = CharSet.Unicode)]
        public static extern int SetWindowTheme(IntPtr window, string app, string ids);
        [DllImport("user32.dll", CharSet = CharSet.Unicode)]
        public static extern IntPtr SendMessage(IntPtr window, int message, IntPtr wparam, string lparam);

        /// Dark scroll bars; nothing happens where Windows has none.
        public static void Darken(Control control)
        {
            try
            {
                SetWindowTheme(control.Handle, "DarkMode_Explorer", null);
            }
            catch (Exception e)
            {
                if (!(e is DllNotFoundException || e is EntryPointNotFoundException)) throw;
            }
        }

        /// A dark title bar.
        public static void DarkenCaption(Form form)
        {
            try
            {
                int on = 1;
                if (DwmSetWindowAttribute(form.Handle, 20, ref on, 4) != 0) DwmSetWindowAttribute(form.Handle, 19, ref on, 4);
            }
            catch (Exception e)
            {
                if (!(e is DllNotFoundException || e is EntryPointNotFoundException)) throw;
            }
        }
    }

    /// Colours, fonts and sizes.
    public static class Look
    {
        public static readonly Color Back = Color.FromArgb(16, 20, 29);
        public static readonly Color Card = Color.FromArgb(25, 31, 43);
        public static readonly Color Sunk = Color.FromArgb(13, 17, 24);
        public static readonly Color Raised = Color.FromArgb(38, 46, 62);
        public static readonly Color Line = Color.FromArgb(44, 53, 70);
        public static readonly Color Ink = Color.FromArgb(233, 240, 250);
        public static readonly Color Soft = Color.FromArgb(177, 189, 208);
        public static readonly Color Faint = Color.FromArgb(134, 147, 168);
        public static readonly Color Accent = Color.FromArgb(109, 222, 204);
        public static readonly Color Good = Color.FromArgb(72, 199, 116);
        public static readonly Color Warn = Color.FromArgb(250, 178, 25);
        public static readonly Color Bad = Color.FromArgb(224, 79, 79);
        public static readonly Color BadInk = Color.FromArgb(255, 143, 143);
        // The parts of a frame: told apart by colour-blind eyes too, and in
        // the order they lie in the bar.
        public static readonly Color Gpu = Color.FromArgb(57, 135, 229);
        public static readonly Color Prepare = Color.FromArgb(217, 89, 38);
        public static readonly Color CopyBack = Color.FromArgb(25, 158, 112);
        public static readonly Color Rest = Color.FromArgb(90, 101, 121);

        public static float Scale = 1;
        public static Font Small, Body, Strong, Head, Value, Big, Mono;

        public static void Init(float scale)
        {
            Scale = Math.Max(0.5f, Math.Min(4, scale));
            Small = Make("Segoe UI", 11.5f, FontStyle.Regular);
            Body = Make("Segoe UI", 12.5f, FontStyle.Regular);
            Strong = Make("Segoe UI Semibold", 13, FontStyle.Regular);
            Head = Make("Segoe UI Semibold", 15, FontStyle.Regular);
            Value = Make("Segoe UI Semibold", 16, FontStyle.Regular);
            Big = Make("Segoe UI Semibold", 50, FontStyle.Regular);
            Mono = Make("Consolas", 12.5f, FontStyle.Regular);
        }

        static Font Make(string family, float size, FontStyle style)
        {
            return new Font(family, size * Scale, style, GraphicsUnit.Pixel);
        }

        public static int Px(double v)
        {
            return (int)Math.Round(v * Scale);
        }

        public static Color Of(Mark mark)
        {
            return mark == Mark.Error ? Bad : mark == Mark.Warn ? Warn : mark == Mark.Good ? Good : mark == Mark.Stage ? Accent : Faint;
        }

        public static Color InkOf(Mark mark)
        {
            return mark == Mark.Error ? BadInk : mark == Mark.Warn ? Warn : mark == Mark.Good ? Good : mark == Mark.Stage ? Accent : Ink;
        }
    }

    static class Draw
    {
        const TextFormatFlags Line = TextFormatFlags.NoPrefix | TextFormatFlags.NoPadding | TextFormatFlags.SingleLine | TextFormatFlags.EndEllipsis;

        public static void Text(Graphics g, string text, Font font, Color color, Rectangle r,
            TextFormatFlags flags = TextFormatFlags.Left | TextFormatFlags.VerticalCenter)
        {
            if (string.IsNullOrEmpty(text) || r.Width <= 0 || r.Height <= 0) return;
            TextRenderer.DrawText(g, text, font, r, color, flags | Line);
        }

        public static void Right(Graphics g, string text, Font font, Color color, Rectangle r)
        {
            Text(g, text, font, color, r, TextFormatFlags.Right | TextFormatFlags.VerticalCenter);
        }

        /// The first font the text fits in, else the last one.
        public static Font Fit(Graphics g, string text, int width, params Font[] fonts)
        {
            foreach (Font font in fonts)
                if (Width(g, text, font) <= width) return font;
            return fonts[fonts.Length - 1];
        }

        public static int Width(Graphics g, string text, Font font)
        {
            if (string.IsNullOrEmpty(text)) return 0;
            return TextRenderer.MeasureText(g, text, font, new Size(int.MaxValue, int.MaxValue),
                TextFormatFlags.NoPrefix | TextFormatFlags.NoPadding | TextFormatFlags.SingleLine).Width;
        }

        static GraphicsPath Path(RectangleF r, float radius)
        {
            GraphicsPath path = new GraphicsPath();
            float d = Math.Min(2 * radius, Math.Min(r.Width, r.Height));
            if (d < 1)
            {
                path.AddRectangle(r);
                return path;
            }
            path.AddArc(r.X, r.Y, d, d, 180, 90);
            path.AddArc(r.Right - d, r.Y, d, d, 270, 90);
            path.AddArc(r.Right - d, r.Bottom - d, d, d, 0, 90);
            path.AddArc(r.X, r.Bottom - d, d, d, 90, 90);
            path.CloseFigure();
            return path;
        }

        public static void Fill(Graphics g, RectangleF r, double radius, Color color)
        {
            if (r.Width <= 0 || r.Height <= 0) return;
            SmoothingMode before = g.SmoothingMode;
            g.SmoothingMode = SmoothingMode.AntiAlias;
            using (GraphicsPath path = Path(r, (float)(radius * Look.Scale)))
            using (SolidBrush brush = new SolidBrush(color)) g.FillPath(brush, path);
            g.SmoothingMode = before;
        }

        public static void Ring(Graphics g, RectangleF r, double radius, Color color)
        {
            SmoothingMode before = g.SmoothingMode;
            g.SmoothingMode = SmoothingMode.AntiAlias;
            r.Inflate(-0.5f, -0.5f);
            using (GraphicsPath path = Path(r, (float)(radius * Look.Scale)))
            using (Pen pen = new Pen(color)) g.DrawPath(pen, path);
            g.SmoothingMode = before;
        }

        /// A track with the part `share` of it filled.
        public static void Bar(Graphics g, Rectangle r, double share, Color color, Color track)
        {
            Fill(g, r, 3, track);
            if (!Say.Known(share) || share <= 0) return;
            float width = (float)(r.Width * Math.Min(1, share));
            Fill(g, new RectangleF(r.X, r.Y, Math.Max(width, Math.Min(r.Width, r.Height)), r.Height), 3, color);
        }

        /// What kind of event: by shape, not only by colour.
        public static void Icon(Graphics g, Mark mark, Rectangle r)
        {
            SmoothingMode before = g.SmoothingMode;
            g.SmoothingMode = SmoothingMode.AntiAlias;
            float x = r.X + r.Width / 2f, y = r.Y + r.Height / 2f, s = Math.Min(r.Width, r.Height) / 2f;
            using (SolidBrush brush = new SolidBrush(Look.Of(mark)))
            using (Pen pen = new Pen(Look.Sunk, Math.Max(1.4f, s * 0.26f)))
            using (Pen line = new Pen(Look.Of(mark), Math.Max(1.8f, s * 0.32f)))
            {
                pen.StartCap = pen.EndCap = line.StartCap = line.EndCap = LineCap.Round;
                line.LineJoin = LineJoin.Round;
                if (mark == Mark.Error)
                {
                    g.FillEllipse(brush, x - s, y - s, 2 * s, 2 * s);
                    g.DrawLine(pen, x - s * 0.38f, y - s * 0.38f, x + s * 0.38f, y + s * 0.38f);
                    g.DrawLine(pen, x + s * 0.38f, y - s * 0.38f, x - s * 0.38f, y + s * 0.38f);
                }
                else if (mark == Mark.Warn)
                {
                    g.FillPolygon(brush, new[] { new PointF(x, y - s), new PointF(x + s * 1.05f, y + s * 0.86f), new PointF(x - s * 1.05f, y + s * 0.86f) });
                    g.DrawLine(pen, x, y - s * 0.3f, x, y + s * 0.2f);
                    g.DrawLine(pen, x, y + s * 0.56f, x, y + s * 0.57f);
                }
                else if (mark == Mark.Good)
                    g.DrawLines(line, new[] { new PointF(x - s * 0.75f, y + s * 0.05f), new PointF(x - s * 0.2f, y + s * 0.6f), new PointF(x + s * 0.8f, y - s * 0.55f) });
                else if (mark == Mark.Stage)
                    g.FillPolygon(brush, new[] { new PointF(x - s * 0.6f, y - s * 0.85f), new PointF(x + s * 0.8f, y), new PointF(x - s * 0.6f, y + s * 0.85f) });
                else g.FillEllipse(brush, x - s * 0.36f, y - s * 0.36f, s * 0.72f, s * 0.72f);
            }
            g.SmoothingMode = before;
        }
    }

    public enum ChipRole { Button, Toggle, Choice }

    /// A button, a check box or one of a set, drawn alike.
    public sealed class Chip : Control
    {
        readonly ChipRole role;
        bool hot, down, on;
        /// Marks of the choice: colour of its count.
        public Mark Mark = Mark.Info;

        public Chip(string text, ChipRole role)
        {
            this.role = role;
            SetStyle(ControlStyles.UserPaint | ControlStyles.AllPaintingInWmPaint | ControlStyles.OptimizedDoubleBuffer
                | ControlStyles.ResizeRedraw | ControlStyles.Selectable | ControlStyles.SupportsTransparentBackColor, true);
            BackColor = Look.Back;
            Text = text;
            TabStop = true;
            Cursor = Cursors.Hand;
            AccessibleRole = role == ChipRole.Toggle ? AccessibleRole.CheckButton
                : role == ChipRole.Choice ? AccessibleRole.RadioButton : AccessibleRole.PushButton;
        }

        public bool Checked
        {
            get { return on; }
            set
            {
                if (on == value) return;
                on = value;
                Tell();
                Invalidate();
            }
        }

        /// What a screen reader says of it.
        void Tell()
        {
            AccessibleName = role == ChipRole.Button ? Text : Text + (on ? ", marcado" : ", desmarcado");
        }

        public int Wanted()
        {
            using (Graphics g = CreateGraphics())
                return Draw.Width(g, Text, Look.Body) + Look.Px(role == ChipRole.Toggle ? 30 : 22);
        }

        protected override void OnTextChanged(EventArgs e)
        {
            base.OnTextChanged(e);
            Tell();
            Invalidate();
        }

        protected override void OnMouseEnter(EventArgs e)
        {
            hot = true;
            Invalidate();
            base.OnMouseEnter(e);
        }

        protected override void OnMouseLeave(EventArgs e)
        {
            hot = down = false;
            Invalidate();
            base.OnMouseLeave(e);
        }

        protected override void OnMouseDown(MouseEventArgs e)
        {
            if (e.Button == MouseButtons.Left)
            {
                down = true;
                Focus();
                Invalidate();
            }
            base.OnMouseDown(e);
        }

        protected override void OnMouseUp(MouseEventArgs e)
        {
            down = false;
            Invalidate();
            base.OnMouseUp(e);
        }

        protected override void OnGotFocus(EventArgs e)
        {
            Invalidate();
            base.OnGotFocus(e);
        }

        protected override void OnLostFocus(EventArgs e)
        {
            Invalidate();
            base.OnLostFocus(e);
        }

        protected override void OnKeyDown(KeyEventArgs e)
        {
            if (e.KeyCode == Keys.Space || e.KeyCode == Keys.Enter)
            {
                e.Handled = true;
                OnClick(EventArgs.Empty);
            }
            base.OnKeyDown(e);
        }

        protected override void OnClick(EventArgs e)
        {
            if (role == ChipRole.Toggle) Checked = !Checked;
            base.OnClick(e);
        }

        protected override void OnPaint(PaintEventArgs e)
        {
            Graphics g = e.Graphics;
            g.Clear(BackColor);
            Rectangle r = ClientRectangle;
            Color ink = Look.Soft;
            if (role == ChipRole.Toggle)
            {
                int box = Look.Px(14);
                Rectangle mark = new Rectangle(Look.Px(4), (r.Height - box) / 2, box, box);
                Draw.Fill(g, mark, 3, on ? Look.Accent : Look.Card);
                if (!on) Draw.Ring(g, mark, 3, hot ? Look.Soft : Look.Faint);
                if (on)
                {
                    SmoothingMode before = g.SmoothingMode;
                    g.SmoothingMode = SmoothingMode.AntiAlias;
                    using (Pen pen = new Pen(Look.Sunk, Math.Max(1.6f, 1.8f * Look.Scale)))
                        g.DrawLines(pen, new[]
                        {
                            new PointF(mark.X + box * 0.22f, mark.Y + box * 0.52f),
                            new PointF(mark.X + box * 0.43f, mark.Y + box * 0.72f),
                            new PointF(mark.X + box * 0.8f, mark.Y + box * 0.3f),
                        });
                    g.SmoothingMode = before;
                }
                Draw.Text(g, Text, Look.Body, hot ? Look.Ink : ink, new Rectangle(mark.Right + Look.Px(6), 0, r.Width - mark.Right - Look.Px(6), r.Height));
            }
            else
            {
                bool lit = role == ChipRole.Choice && on;
                Color fill = lit ? Look.Raised : down ? Look.Raised : hot ? Look.Line : Look.Card;
                Draw.Fill(g, r, 5, fill);
                if (role == ChipRole.Button) Draw.Ring(g, r, 5, Look.Line);
                if (lit) ink = Look.Ink;
                if (hot) ink = Look.Ink;
                Draw.Text(g, Text, Look.Body, ink, r, TextFormatFlags.HorizontalCenter | TextFormatFlags.VerticalCenter);
                if (lit)
                    Draw.Fill(g, new Rectangle(r.X + Look.Px(10), r.Bottom - Look.Px(3), r.Width - Look.Px(20), Look.Px(2)), 1,
                        Mark == Mark.Info ? Look.Accent : Look.Of(Mark));
            }
            if (Focused && ShowFocusCues) Draw.Ring(g, r, 5, Look.Accent);
        }
    }

    /// What a list shows: rows drawn when they are seen.
    public abstract class Rows
    {
        public abstract int Count { get; }
        public abstract void Paint(Graphics g, int row, Rectangle r, bool selected);
        public abstract string Copy(int row);
    }

    /// A list of rows of one height, of any length.
    public sealed class RowList : Control
    {
        readonly VScrollBar bar = new VScrollBar();
        Rows rows;
        int top, anchor = -1, focus = -1;
        bool follow = true, filling;
        int wheel;
        public event EventHandler SelectionChanged;
        public event EventHandler FollowChanged;
        public string Empty = "";

        public RowList()
        {
            SetStyle(ControlStyles.UserPaint | ControlStyles.AllPaintingInWmPaint | ControlStyles.OptimizedDoubleBuffer
                | ControlStyles.ResizeRedraw | ControlStyles.Selectable, true);
            BackColor = Look.Sunk;
            TabStop = true;
            AccessibleRole = AccessibleRole.List;
            bar.Dock = DockStyle.Right;
            bar.TabStop = false;
            bar.Scroll += (sender, e) =>
            {
                if (filling) return;
                top = e.NewValue;
                Followed();
                Invalidate();
            };
            Controls.Add(bar);
            bar.HandleCreated += (sender, e) => Native.Darken(bar);
        }

        public static int RowHeight { get { return Look.Px(22); } }

        int Shown { get { return Math.Max(1, (ClientSize.Height - Look.Px(4)) / RowHeight); } }
        int Last { get { return Math.Max(0, (rows == null ? 0 : rows.Count) - Shown); } }

        public Rows Source
        {
            get { return rows; }
            set
            {
                rows = value;
                anchor = focus = -1;
                Refill(0);
                Notify();
            }
        }

        public bool Follow
        {
            get { return follow; }
            set
            {
                if (follow == value) return;
                follow = value;
                Refill(0);
                if (FollowChanged != null) FollowChanged(this, EventArgs.Empty);
            }
        }

        public int Current { get { return focus; } }

        /// The rows changed; `removed` went away at the front.
        public void Refill(int removed)
        {
            int count = rows == null ? 0 : rows.Count;
            if (removed > 0)
            {
                top = Math.Max(0, top - removed);
                if (focus >= 0) focus -= removed;
                if (anchor >= 0) anchor = Math.Max(0, anchor - removed);
                if (focus < 0) anchor = focus = -1;
            }
            if (focus >= count) anchor = focus = -1;
            if (anchor >= count) anchor = focus;
            top = follow ? Last : Math.Min(top, Last);
            filling = true;
            bar.Minimum = 0;
            bar.SmallChange = 1;
            bar.LargeChange = Shown;
            bar.Maximum = Math.Max(0, count - 1);
            bar.Value = Math.Max(0, Math.Min(top, Math.Max(0, bar.Maximum - bar.LargeChange + 1)));
            bar.Enabled = count > Shown;
            filling = false;
            Invalidate();
        }

        void Followed()
        {
            bool now = top >= Last;
            if (now == follow) return;
            follow = now;
            if (FollowChanged != null) FollowChanged(this, EventArgs.Empty);
        }

        void Notify()
        {
            if (SelectionChanged != null) SelectionChanged(this, EventArgs.Empty);
        }

        public void ClearSelection()
        {
            if (focus < 0) return;
            anchor = focus = -1;
            Invalidate();
            Notify();
        }

        void Select(int row, bool extend)
        {
            int count = rows == null ? 0 : rows.Count;
            if (count == 0) return;
            row = Math.Max(0, Math.Min(count - 1, row));
            focus = row;
            if (!extend || anchor < 0) anchor = row;
            if (row < top) top = row;
            if (row >= top + Shown) top = row - Shown + 1;
            Followed();
            Refill(0);
            Notify();
        }

        /// The text of the rows selected.
        public string Copied()
        {
            if (rows == null || focus < 0) return "";
            System.Text.StringBuilder text = new System.Text.StringBuilder();
            for (int k = Math.Min(anchor, focus); k <= Math.Max(anchor, focus) && k < rows.Count; k++) text.AppendLine(rows.Copy(k));
            return text.ToString();
        }

        protected override void OnResize(EventArgs e)
        {
            base.OnResize(e);
            Refill(0);
        }

        protected override bool IsInputKey(Keys key)
        {
            switch (key & Keys.KeyCode)
            {
                case Keys.Up:
                case Keys.Down:
                case Keys.PageUp:
                case Keys.PageDown:
                case Keys.Home:
                case Keys.End:
                    return true;
            }
            return base.IsInputKey(key);
        }

        protected override void OnKeyDown(KeyEventArgs e)
        {
            int at = focus < 0 ? top : focus;
            bool mine = true;
            switch (e.KeyCode)
            {
                case Keys.Up: Select(at - 1, e.Shift); break;
                case Keys.Down: Select(focus < 0 ? top : at + 1, e.Shift); break;
                case Keys.PageUp: Select(at - Shown, e.Shift); break;
                case Keys.PageDown: Select(at + Shown, e.Shift); break;
                case Keys.Home: Select(0, e.Shift); break;
                case Keys.End: Select(int.MaxValue, e.Shift); break;
                case Keys.Escape: ClearSelection(); break;
                case Keys.A:
                    if (e.Control && rows != null && rows.Count > 0)
                    {
                        anchor = 0;
                        focus = rows.Count - 1;
                        Invalidate();
                        Notify();
                    }
                    break;
                default: mine = false; break;
            }
            e.Handled |= mine;
            base.OnKeyDown(e);
        }

        protected override void OnMouseDown(MouseEventArgs e)
        {
            Focus();
            int row = top + (e.Y - Look.Px(2)) / RowHeight;
            if (rows != null && row >= 0 && row < rows.Count)
            {
                bool shift = (ModifierKeys & Keys.Shift) != 0;
                if (e.Button == MouseButtons.Right && focus >= 0 && row >= Math.Min(anchor, focus) && row <= Math.Max(anchor, focus)) { }
                else if (row == focus && anchor == focus && !shift && e.Button == MouseButtons.Left) ClearSelection();
                else Select(row, shift);
            }
            base.OnMouseDown(e);
        }

        protected override void OnMouseMove(MouseEventArgs e)
        {
            if (e.Button == MouseButtons.Left && focus >= 0)
            {
                int row = top + (int)Math.Floor((double)(e.Y - Look.Px(2)) / RowHeight);
                if (row != focus) Select(row, true);
            }
            base.OnMouseMove(e);
        }

        protected override void OnMouseWheel(MouseEventArgs e)
        {
            wheel += e.Delta;
            int steps = wheel / 120;
            wheel -= steps * 120;
            if (steps != 0)
            {
                top = Math.Max(0, Math.Min(Last, top - 3 * steps));
                Followed();
                Refill(0);
            }
            base.OnMouseWheel(e);
        }

        protected override void OnGotFocus(EventArgs e)
        {
            Invalidate();
            base.OnGotFocus(e);
        }

        protected override void OnLostFocus(EventArgs e)
        {
            Invalidate();
            base.OnLostFocus(e);
        }

        protected override void OnPaint(PaintEventArgs e)
        {
            Graphics g = e.Graphics;
            g.Clear(BackColor);
            int width = ClientSize.Width - bar.Width, height = RowHeight, y = Look.Px(2);
            int count = rows == null ? 0 : rows.Count;
            if (count == 0)
            {
                Draw.Text(g, Empty, Look.Body, Look.Faint, new Rectangle(Look.Px(12), Look.Px(8), width - Look.Px(24), Look.Px(22)));
                return;
            }
            int from = Math.Min(anchor, focus), to = Math.Max(anchor, focus);
            for (int row = top; row < count && row < top + Shown; row++, y += height)
            {
                Rectangle r = new Rectangle(0, y, width, height);
                bool selected = focus >= 0 && row >= from && row <= to;
                if (selected)
                    using (SolidBrush brush = new SolidBrush(Focused ? Look.Raised : Look.Card)) g.FillRectangle(brush, r);
                try
                {
                    rows.Paint(g, row, r, selected);
                }
                catch (Exception x)
                {
                    Faults.Note("linha da lista", x);
                }
            }
            if (Focused && ShowFocusCues) Draw.Ring(g, new Rectangle(0, 0, width, ClientSize.Height), 2, Look.Line);
        }
    }
}
namespace Vyce.Monitor
{
    using System;
    using System.Collections.Generic;
    using System.Drawing;
    using System.Drawing.Drawing2D;
    using System.Windows.Forms;

    /// The numbers of the run, drawn: frame rate, loading, frame time, tiles.
    public sealed class Dashboard : Control
    {
        readonly ToolTip tip = new ToolTip();
        readonly List<KeyValuePair<Rectangle, string>> tips = new List<KeyValuePair<Rectangle, string>>();
        Shot shot = new Shot();
        Rectangle rate, graph, band, frame, plot;
        Rectangle[] tiles = new Rectangle[0];
        string told;
        Point pointer = new Point(-1, -1);

        public Dashboard()
        {
            SetStyle(ControlStyles.UserPaint | ControlStyles.AllPaintingInWmPaint | ControlStyles.OptimizedDoubleBuffer
                | ControlStyles.ResizeRedraw, true);
            BackColor = Look.Back;
            AccessibleRole = AccessibleRole.StaticText;
            AccessibleName = "Números do jogo";
            tip.InitialDelay = 350;
            tip.ReshowDelay = 100;
            tip.AutoPopDelay = 20000;
        }

        public Shot Shot
        {
            get { return shot; }
            set
            {
                shot = value;
                Invalidate();
            }
        }

        /// Roomy windows get a taller graph. Beside the log (`brief`), a low
        /// one leaves the lower half out.
        public static int Wanted(int window, bool brief)
        {
            if (Tall(window)) return Look.Px(150 + 8 + 74 + 8 + 92 + 8 + 70);
            return Look.Px(brief ? 118 + 6 + 66 : 118 + 6 + 66 + 6 + 84 + 6 + 64);
        }

        static bool Tall(int window)
        {
            return window >= Look.Px(860);
        }

        /// Places the parts for a window of this height.
        public void Arrange(int window, bool brief)
        {
            bool tall = Tall(window);
            int gap = Look.Px(tall ? 8 : 6), width = ClientSize.Width, y = 0;
            int h = Look.Px(tall ? 150 : 118);
            int left = Math.Min(Look.Px(300), width * 36 / 100);
            rate = new Rectangle(0, y, left, h);
            graph = new Rectangle(left + gap, y, width - left - gap, h);
            y += h + gap;
            h = Look.Px(tall ? 74 : 66);
            band = new Rectangle(0, y, width, h);
            y += h + gap;
            if (brief && !tall)
            {
                frame = Rectangle.Empty;
                tiles = new Rectangle[0];
                return;
            }
            h = Look.Px(tall ? 92 : 84);
            frame = new Rectangle(0, y, width, h);
            y += h + gap;
            h = Look.Px(tall ? 70 : 64);
            // The first two hold two numbers each.
            double[] shares = { 1.16, 1.16, 0.86, 1, 0.86, 0.98, 1.02, 1.02 };
            double all = 0, before = 0;
            foreach (double share in shares) all += share;
            tiles = new Rectangle[shares.Length];
            for (int k = 0; k < shares.Length; k++)
            {
                int from = (int)(before / all * (width + gap));
                before += shares[k];
                tiles[k] = new Rectangle(from, y, (int)(before / all * (width + gap)) - gap - from, h);
            }
        }

        /// One part that fails leaves the others alone.
        void Part(string name, Rectangle r, Graphics g, Action<Graphics, Rectangle> draw)
        {
            GraphicsState state = g.Save();
            try
            {
                draw(g, r);
            }
            catch (Exception e)
            {
                Faults.Note(name, e);
                Draw.Text(g, "esta parte falhou: " + e.Message, Look.Small, Look.Faint, Rectangle.Inflate(r, -Look.Px(10), 0));
            }
            g.Restore(state);
        }

        protected override void OnPaint(PaintEventArgs e)
        {
            Graphics g = e.Graphics;
            g.Clear(BackColor);
            tips.Clear();
            Part("FPS", rate, g, Rate);
            Part("gráfico de FPS", graph, g, Graph);
            Part("carregamento", band, g, Band);
            if (frame.Height > 0) Part("tempo do quadro", frame, g, Frame);
            for (int k = 0; k < tiles.Length && k < shot.Tiles.Length; k++)
            {
                Tile tile = shot.Tiles[k];
                Part(tile.Caption, tiles[k], g, (to, r) => Tile(to, r, tile));
            }
        }

        static void Card(Graphics g, Rectangle r)
        {
            Draw.Fill(g, r, 7, Look.Card);
        }

        void Tip(Rectangle r, string text)
        {
            if (!string.IsNullOrEmpty(text)) tips.Add(new KeyValuePair<Rectangle, string>(r, text));
        }

        void Rate(Graphics g, Rectangle r)
        {
            Card(g, r);
            Rectangle inner = Rectangle.Inflate(r, -Look.Px(14), -Look.Px(9));
            int line = Look.Px(18);
            bool fresh = shot.Alive && !shot.Stale && shot.Last != null;
            Draw.Text(g, "FPS", Look.Strong, Look.Soft, new Rectangle(inner.X, inner.Y, inner.Width, line));
            if (shot.AgeText != "")
            {
                Color ink = shot.Stale ? Look.Warn : Look.Soft;
                int width = Draw.Width(g, shot.AgeText, Look.Body);
                Draw.Right(g, shot.AgeText, Look.Body, ink, new Rectangle(inner.X, inner.Y, inner.Width, line));
                Rectangle dot = new Rectangle(inner.Right - width - Look.Px(15), inner.Y + (line - Look.Px(8)) / 2, Look.Px(8), Look.Px(8));
                Draw.Fill(g, dot, 4, shot.Stale ? Look.Warn : fresh ? Look.Good : Look.Faint);
            }
            Rectangle number = new Rectangle(inner.X, inner.Y + line, inner.Width, inner.Height - 2 * line);
            string fps = shot.Last == null ? Say.Nothing : Say.Fps(shot.Last.Fps);
            Draw.Text(g, fps, Look.Big, fresh ? Look.Ink : Look.Faint, number);
            if (shot.Last != null)
            {
                int from = number.X + Draw.Width(g, fps, Look.Big) + Look.Px(16);
                Rectangle beside = new Rectangle(from, number.Y + number.Height / 2 - Look.Px(17), inner.Right - from, Look.Px(38));
                Draw.Text(g, Say.Ms(shot.Last.Ms), Look.Value, fresh ? Look.Ink : Look.Faint, new Rectangle(beside.X, beside.Y, beside.Width, Look.Px(20)));
                Draw.Text(g, "por quadro", Look.Small, Look.Soft, new Rectangle(beside.X, beside.Y + Look.Px(20), beside.Width, Look.Px(16)));
            }
            Draw.Text(g, shot.Note, shot.Stale ? Draw.Fit(g, shot.Note, inner.Width, Look.Strong, Look.Body, Look.Small) : Look.Small,
                shot.Stale ? Look.Warn : Look.Soft, new Rectangle(inner.X, inner.Bottom - line, inner.Width, line));
            Tip(r, "Quadros por segundo no último segundo medido.\n" + (shot.Stale
                ? "O número está velho: o jogo não apresentou nada depois dele."
                : "30 já é fluido para este jogo; abaixo de 10 fica difícil de jogar."));
        }

        static double Ceiling(double v)
        {
            foreach (double step in new double[] { 2, 5, 10, 15, 20, 30, 40, 60, 80, 120, 160, 240, 360, 500 })
                if (v <= step) return step;
            return Math.Ceiling(v / 500) * 500;
        }

        /// The top of the axis: the highest rate but for short peaks.
        double AxisTop()
        {
            List<KeyValuePair<double, double>> rates = new List<KeyValuePair<double, double>>();
            double all = 0;
            foreach (Sample s in shot.Recent)
            {
                double seconds = Math.Min(10, s.Frames * s.Ms / 1000);
                rates.Add(new KeyValuePair<double, double>(s.Fps, seconds));
                all += seconds;
            }
            rates.Sort((a, b) => a.Key.CompareTo(b.Key));
            double passed = 0;
            foreach (KeyValuePair<double, double> rate in rates)
            {
                passed += rate.Value;
                if (passed >= 0.96 * all) return Ceiling(Math.Max(2, rate.Key * 1.1));
            }
            return 30;
        }

        float X(DateTime at)
        {
            double back = (shot.Now - at).TotalSeconds / (60 * Shot.Minutes);
            return (float)(plot.Right - back * plot.Width);
        }

        void Graph(Graphics g, Rectangle r)
        {
            Card(g, r);
            Rectangle inner = Rectangle.Inflate(r, -Look.Px(14), -Look.Px(9));
            int line = Look.Px(18);
            Draw.Text(g, "FPS nos últimos 2 minutos", Look.Strong, Look.Soft, new Rectangle(inner.X, inner.Y, inner.Width, line));
            double top = AxisTop();
            string[] marks = { Say.Number(top, "0"), Say.Number(top / 2, top / 2 == Math.Floor(top / 2) ? "0" : "0.0"), "0" };
            int gutter = 0;
            foreach (string mark in marks) gutter = Math.Max(gutter, Draw.Width(g, mark, Look.Small));
            gutter += Look.Px(8);
            plot = new Rectangle(inner.X + gutter, inner.Y + line + Look.Px(8),
                inner.Width - gutter - Look.Px(4), inner.Height - 2 * line - Look.Px(8));
            using (Pen grid = new Pen(Look.Line))
            {
                for (int k = 0; k < 3; k++)
                {
                    int y = plot.Y + k * plot.Height / 2;
                    g.DrawLine(grid, plot.X, y, plot.Right, y);
                    Draw.Right(g, marks[k], Look.Small, Look.Faint, new Rectangle(inner.X, y - Look.Px(8), gutter - Look.Px(8), Look.Px(16)));
                }
            }
            int foot = inner.Bottom - line + Look.Px(3);
            Draw.Text(g, "2 min atrás", Look.Small, Look.Faint, new Rectangle(plot.X, foot, Look.Px(120), line));
            Draw.Text(g, "1 min", Look.Small, Look.Faint, new Rectangle(plot.X, foot, plot.Width, line),
                TextFormatFlags.HorizontalCenter | TextFormatFlags.VerticalCenter);
            Draw.Right(g, shot.Alive ? "agora" : "fim", Look.Small, Look.Faint, new Rectangle(plot.X, foot, plot.Width, line));
            if (shot.Recent.Length == 0)
            {
                Draw.Text(g, "ainda sem quadros", Look.Body, Look.Faint, plot, TextFormatFlags.HorizontalCenter | TextFormatFlags.VerticalCenter);
                return;
            }
            g.SetClip(Rectangle.FromLTRB(plot.X, plot.Y - Look.Px(3), plot.Right + Look.Px(5), plot.Bottom + 1));
            List<PointF> points = new List<PointF>();
            DateTime before = DateTime.MinValue;
            PointF end = PointF.Empty;
            foreach (Sample s in shot.Recent)
            {
                // Nothing was shown in between: no line either.
                double covered = s.Frames * s.Ms / 1000;
                if (points.Count > 0 && (s.At - before).TotalSeconds > Math.Max(Shot.Old, covered + 2))
                {
                    Curve(g, points);
                    Hole(g, before, s.At);
                    points.Clear();
                }
                end = new PointF(X(s.At), (float)(plot.Bottom - plot.Height * Math.Min(1, s.Fps / top)));
                points.Add(end);
                before = s.At;
            }
            Curve(g, points);
            if (shot.Stale) Hole(g, before, shot.Now);
            float dot = Look.Px(8);
            Draw.Fill(g, new RectangleF(end.X - dot / 2 - 2, end.Y - dot / 2 - 2, dot + 4, dot + 4), 8, Look.Card);
            Draw.Fill(g, new RectangleF(end.X - dot / 2, end.Y - dot / 2, dot, dot), 8, shot.Stale || !shot.Alive ? Look.Faint : Look.Accent);
            Cross(g, top);
        }

        void Curve(Graphics g, List<PointF> points)
        {
            if (points.Count < 2) return;
            SmoothingMode smooth = g.SmoothingMode;
            g.SmoothingMode = SmoothingMode.AntiAlias;
            PointF[] line = points.ToArray();
            PointF[] area = new PointF[line.Length + 2];
            line.CopyTo(area, 0);
            area[line.Length] = new PointF(line[line.Length - 1].X, plot.Bottom);
            area[line.Length + 1] = new PointF(line[0].X, plot.Bottom);
            using (SolidBrush wash = new SolidBrush(Color.FromArgb(28, Look.Accent))) g.FillPolygon(wash, area);
            using (Pen pen = new Pen(Look.Accent, 2 * Look.Scale))
            {
                pen.LineJoin = LineJoin.Round;
                pen.StartCap = pen.EndCap = LineCap.Round;
                g.DrawLines(pen, line);
            }
            g.SmoothingMode = smooth;
        }

        /// Seconds without a frame.
        void Hole(Graphics g, DateTime from, DateTime to)
        {
            float x = Math.Max(plot.X, X(from)), end = Math.Min(plot.Right, X(to));
            if (end - x < 2) return;
            using (SolidBrush brush = new SolidBrush(Color.FromArgb(34, Look.Warn))) g.FillRectangle(brush, x, plot.Y, end - x, plot.Height);
        }

        Sample Nearest()
        {
            if (!plot.Contains(pointer) || shot.Recent.Length == 0) return null;
            Sample best = null;
            float least = float.MaxValue;
            foreach (Sample s in shot.Recent)
            {
                float away = Math.Abs(X(s.At) - pointer.X);
                if (away >= least) continue;
                least = away;
                best = s;
            }
            return best;
        }

        void Cross(Graphics g, double top)
        {
            Sample s = Nearest();
            if (s == null) return;
            float x = X(s.At), y = (float)(plot.Bottom - plot.Height * Math.Min(1, s.Fps / top));
            using (Pen pen = new Pen(Look.Faint)) g.DrawLine(pen, x, plot.Y, x, plot.Bottom);
            float dot = Look.Px(8);
            Draw.Fill(g, new RectangleF(x - dot / 2, y - dot / 2, dot, dot), 8, Look.Ink);
        }

        void Band(Graphics g, Rectangle r)
        {
            Card(g, r);
            Band b = shot.Band;
            Rectangle inner = Rectangle.Inflate(r, -Look.Px(14), -Look.Px(8));
            int line = Look.Px(20), bar = Look.Px(8);
            bool warn = b.Mark == Mark.Warn;
            int right = Draw.Width(g, b.Right, Look.Head);
            Draw.Right(g, b.Right, Look.Head, Look.Soft, new Rectangle(inner.X, inner.Y, inner.Width, line));
            Draw.Text(g, b.Title, Look.Head, warn ? Look.Warn : b.Mark == Mark.Error ? Look.BadInk : b.Mark == Mark.Info ? Look.Soft : Look.Ink,
                new Rectangle(inner.X, inner.Y, inner.Width - (right > 0 ? right + Look.Px(16) : 0), line));
            if (Say.Known(b.Share))
                Draw.Bar(g, new Rectangle(inner.X, inner.Y + line + Look.Px(3), inner.Width, bar), b.Share, warn ? Look.Warn : Look.Accent, Look.Sunk);
            else
                Draw.Bar(g, new Rectangle(inner.X, inner.Y + line + Look.Px(5), inner.Width, Look.Px(3)), double.NaN, Look.Accent, Look.Sunk);
            Draw.Text(g, b.Detail, Look.Body, Look.Soft, new Rectangle(inner.X, inner.Bottom - Look.Px(18), inner.Width, Look.Px(18)));
            Tip(r, b.Title + (b.Right == "" ? "" : "  (" + b.Right + ")") + "\n" + b.Detail.Replace("  ·  ", "\n"));
        }

        struct Piece
        {
            public string Name, Tip;
            public double Ms;
            public Color Color;
        }

        double V(string name)
        {
            return shot.Last == null ? double.NaN : shot.Last[name];
        }

        static void Add(List<string> lines, string text, double value, Func<double, string> say)
        {
            if (Say.Known(value) && value >= 0.05) lines.Add(text + say(value));
        }

        Piece[] Pieces(Parts p)
        {
            List<string> gpu = new List<string>();
            Add(gpu, "A GPU trabalha ", p.Busy, v => Say.Ms(v) + " por quadro (" + Say.Share(Math.Min(1, v / p.Ms)) + ")");
            Add(gpu, "Envios por quadro: ", V("submissions_per_frame"), v => Say.Number(v, "0.0"));
            Add(gpu, "Envios sem espera: ", V("unwaited_per_frame"), v => Say.Number(v, "0.0"));
            if (shot.GpuTop != null) gpu.Add("Mais pesado: " + shot.GpuTop);
            List<string> prepare = new List<string>();
            Add(prepare, "Cópias de memória: ", V("cpu_copy_ms"), v => Say.Ms(v) + ", " + Say.Size(V("cpu_copy_mb")));
            Add(prepare, "Texturas: ", V("texture_refresh_ms"), v => Say.Ms(v) + ", " + Say.Size(V("texture_refresh_mb")));
            Add(prepare, "Descritores: ", V("bindless_plan_ms"), Say.Ms);
            Add(prepare, "Cópias de referência: ", V("snapshot_ms"), Say.Ms);
            Add(prepare, "Trabalho adiado: ", V("steps_ms"), Say.Ms);
            Add(prepare, "Buffers na placa: ", V("card_record_ms"), Say.Ms);
            List<string> back = new List<string>();
            Add(back, "Escritos na memória do jogo: ", V("guest_write_mb"), Say.Size);
            List<string> rest = new List<string>();
            Add(rest, "Espera por pipelines: ", p.PipelineWait, Say.Ms);
            Func<string, List<string>, string> tip = (what, lines) => what + "\n" + string.Join("\n", lines.ToArray());
            return new[]
            {
                new Piece { Name = "GPU", Ms = p.Gpu, Color = Look.Gpu,
                    Tip = tip("GPU: o tempo em que o quadro só espera a placa de vídeo.", gpu) },
                new Piece { Name = "Preparo na CPU", Ms = p.Prepare, Color = Look.Prepare,
                    Tip = tip("Preparo na CPU: o que o Vyce faz antes de entregar trabalho à GPU.", prepare) },
                new Piece { Name = "Cópia de volta", Ms = p.Back, Color = Look.CopyBack,
                    Tip = tip("Cópia de volta: o que a GPU escreveu volta para a memória do jogo.", back) },
                new Piece { Name = "Resto", Ms = p.Rest, Color = Look.Rest,
                    Tip = tip("Resto: o próprio jogo, a apresentação e as outras esperas.", rest) },
            };
        }

        void Frame(Graphics g, Rectangle r)
        {
            Card(g, r);
            Rectangle inner = Rectangle.Inflate(r, -Look.Px(14), -Look.Px(8));
            int line = Look.Px(20);
            Parts p = shot.Parts;
            string caption = p == null ? "" : "quadro de " + Say.Ms(p.Ms);
            int right = Draw.Width(g, caption, Look.Body);
            Draw.Right(g, caption, Look.Body, Look.Soft, new Rectangle(inner.X, inner.Y, inner.Width, line));
            Rectangle title = new Rectangle(inner.X, inner.Y, inner.Width - right - Look.Px(16), line);
            if (p == null)
            {
                Draw.Text(g, "Para onde vai o tempo do quadro", Look.Head, Look.Soft, title);
                Draw.Text(g, "Aparece com o primeiro quadro.", Look.Body, Look.Faint, new Rectangle(inner.X, inner.Y + line + Look.Px(6), inner.Width, line));
                return;
            }
            Draw.Text(g, shot.Verdict, Look.Head, shot.Alive ? Look.Ink : Look.Soft, title);
            Piece[] pieces = Pieces(p);
            Rectangle bar = new Rectangle(inner.X, inner.Y + line + Look.Px(4), inner.Width, Look.Px(20));
            float x = bar.X, gap = Math.Max(2, 2 * Look.Scale);
            foreach (Piece piece in pieces)
            {
                float width = (float)(bar.Width * piece.Ms / p.Ms);
                if (width >= 1)
                {
                    RectangleF cell = new RectangleF(x, bar.Y, Math.Max(1, width - gap), bar.Height);
                    Draw.Fill(g, cell, 4, piece.Color);
                    string share = Say.Share(piece.Ms / p.Ms);
                    if (Draw.Width(g, share, Look.Strong) + Look.Px(14) <= cell.Width)
                    {
                        RectangleF label = new RectangleF(cell.X + Look.Px(7), cell.Y, cell.Width - Look.Px(8), cell.Height);
                        Draw.Text(g, share, Look.Strong, Color.White, Rectangle.Round(label));
                    }
                    Tip(Rectangle.Round(cell), piece.Tip.TrimEnd('\n'));
                }
                x += width;
            }
            // Names and times, in the order of the bar.
            int at = inner.X, y = inner.Bottom - Look.Px(18), swatch = Look.Px(10);
            foreach (Piece piece in pieces)
            {
                string time = Say.Ms(piece.Ms);
                Draw.Fill(g, new Rectangle(at, y + Look.Px(5), swatch, swatch), 2.5, piece.Color);
                int from = at + swatch + Look.Px(6);
                int name = Draw.Width(g, piece.Name, Look.Body), wide = Draw.Width(g, time, Look.Strong);
                Draw.Text(g, piece.Name, Look.Body, Look.Soft, new Rectangle(from, y, name + 2, Look.Px(18)));
                Draw.Text(g, time, Look.Strong, Look.Ink, new Rectangle(from + name + Look.Px(6), y, wide + 2, Look.Px(18)));
                Tip(new Rectangle(at, y, from + name + wide - at, Look.Px(18)), piece.Tip.TrimEnd('\n'));
                at = from + name + wide + Look.Px(26);
            }
            if (Say.Known(p.Busy))
            {
                string busy = "GPU ocupada " + Say.Ms(p.Busy);
                if (at + Draw.Width(g, busy, Look.Body) <= inner.Right)
                    Draw.Right(g, busy, Look.Body, Look.Soft, new Rectangle(at, y, inner.Right - at, Look.Px(18)));
            }
        }

        void Tile(Graphics g, Rectangle r, Tile tile)
        {
            Card(g, r);
            Rectangle inner = Rectangle.Inflate(r, -Look.Px(10), -Look.Px(6));
            int small = Look.Px(15);
            Draw.Text(g, tile.Caption, Look.Small, Look.Soft, new Rectangle(inner.X, inner.Y, inner.Width, small));
            Color ink = tile.Mark == Mark.Error ? Look.BadInk : tile.Mark == Mark.Warn ? Look.Warn : Look.Ink;
            bool bar = Say.Known(tile.Share);
            Rectangle value = new Rectangle(inner.X, inner.Y + small, inner.Width, inner.Height - 2 * small - (bar ? Look.Px(2) : 0));
            Draw.Text(g, tile.Value, Draw.Fit(g, tile.Value, inner.Width, Look.Value, Look.Strong), ink, value);
            if (bar)
                Draw.Bar(g, new Rectangle(inner.X, value.Bottom, inner.Width, Look.Px(4)), tile.Share,
                    tile.Mark == Mark.Error ? Look.Bad : tile.Mark == Mark.Warn ? Look.Warn : Look.Accent, Look.Sunk);
            Draw.Text(g, tile.Sub, Look.Small, Look.Faint, new Rectangle(inner.X, inner.Bottom - small + Look.Px(2), inner.Width, small));
            Tip(r, tile.Tip);
        }

        protected override void OnMouseMove(MouseEventArgs e)
        {
            base.OnMouseMove(e);
            pointer = e.Location;
            string text = null;
            Sample s = Nearest();
            if (s != null)
                text = Say.Clock(s.At) + "\n" + Say.Fps(s.Fps) + " FPS, " + Say.Ms(s.Ms) + " por quadro\n" + Say.Count(s.Draws) + " desenhos";
            else
            {
                // The last one drawn lies on top.
                foreach (KeyValuePair<Rectangle, string> region in tips)
                    if (region.Key.Contains(pointer)) text = region.Value;
            }
            if (graph.Contains(pointer)) Invalidate(graph);
            if (text == told) return;
            told = text;
            tip.SetToolTip(this, text ?? "");
        }

        protected override void OnMouseLeave(EventArgs e)
        {
            base.OnMouseLeave(e);
            pointer = new Point(-1, -1);
            Invalidate(graph);
        }

        protected override void Dispose(bool disposing)
        {
            if (disposing) tip.Dispose();
            base.Dispose(disposing);
        }
    }
}
namespace Vyce.Monitor
{
    using System;
    using System.Collections.Generic;
    using System.Diagnostics;
    using System.Drawing;
    using System.IO;
    using System.Text;
    using System.Windows.Forms;

    /// The events, newest last.
    sealed class EventRows : Rows
    {
        readonly Session session;
        public readonly List<Row> View = new List<Row>();
        /// The least mark shown: everything, warnings or errors.
        public Mark Least = Mark.Info;

        public EventRows(Session session)
        {
            this.session = session;
        }

        /// Takes the rows over; the caller holds the session's lock.
        public int Update()
        {
            Row first = View.Count > 0 ? View[0] : null;
            List<Row> rows = session.Journal.Rows;
            int before = View.Count;
            List<Row> old = new List<Row>(View);
            View.Clear();
            foreach (Row row in rows)
                if (row.Kind.Mark >= Least || Least == Mark.Warn && row.Kind.Mark == Mark.Stage) View.Add(row);
            if (first == null || View.Count > 0 && View[0] == first) return 0;
            // How many went away at the front.
            int removed = 0;
            while (removed < before && !View.Contains(old[removed])) removed++;
            return removed;
        }

        public override int Count { get { return View.Count; } }

        string Told(int row)
        {
            lock (session.Sync) return View[row].Text();
        }

        public override void Paint(Graphics g, int row, Rectangle r, bool selected)
        {
            Row e = View[row];
            Mark mark = e.Kind.Mark;
            int x = r.X + Look.Px(10), time = Look.Px(58), icon = Look.Px(13);
            if (mark == Mark.Stage && !selected)
                using (SolidBrush brush = new SolidBrush(Color.FromArgb(24, Look.Accent))) g.FillRectangle(brush, r);
            Draw.Text(g, Say.Clock(e.First), Look.Body, Look.Faint, new Rectangle(x, r.Y, time, r.Height));
            x += time + Look.Px(6);
            Draw.Icon(g, mark, new Rectangle(x, r.Y + (r.Height - icon) / 2, icon, icon));
            x += icon + Look.Px(10);
            Color ink = mark == Mark.Error ? Look.BadInk : mark == Mark.Warn ? Color.FromArgb(246, 222, 170) : Look.Ink;
            Draw.Text(g, Told(row), mark == Mark.Stage ? Look.Strong : Look.Body, ink, new Rectangle(x, r.Y, r.Right - x - Look.Px(8), r.Height));
        }

        public override string Copy(int row)
        {
            return Say.Clock(View[row].First) + "  " + Told(row);
        }

        public string Detail(int row)
        {
            lock (session.Sync)
            {
                Row e = View[row];
                StringBuilder text = new StringBuilder();
                text.Append(Say.Clock(e.First));
                if ((e.Last - e.First).TotalSeconds >= 1) text.Append(" a " + Say.Clock(e.Last));
                text.AppendLine("  " + e.Text());
                if (e.Kind.Why != null) text.AppendLine(e.Kind.Why);
                if (e.Kept.Count > 0)
                {
                    text.AppendLine();
                    text.AppendLine("No log:");
                    foreach (string line in e.Kept) text.AppendLine(line);
                    if (e.Left > 0) text.AppendLine("e mais " + Say.Count(e.Left) + " linhas no arquivo de log");
                }
                return text.ToString().TrimEnd();
            }
        }
    }

    /// The logs' own lines, cleaned.
    sealed class LogRows : Rows
    {
        readonly Session session;
        readonly List<long> view = new List<long>();
        long next;
        bool all;
        string[] words = new string[0];

        public LogRows(Session session)
        {
            this.session = session;
        }

        public void Filter(bool everything, string text)
        {
            all = everything;
            words = text.Split(new[] { ' ' }, StringSplitOptions.RemoveEmptyEntries);
            view.Clear();
            next = 0;
        }

        bool Wanted(LogLine line)
        {
            if (line.Noise && !all) return false;
            foreach (string word in words)
                if (line.Text.IndexOf(word, StringComparison.OrdinalIgnoreCase) < 0) return false;
            return true;
        }

        /// Takes new lines over; how many went away at the front. The caller
        /// holds the session's lock.
        public int Update()
        {
            Ring log = session.Log;
            int removed = 0;
            while (removed < view.Count && view[removed] < log.First) removed++;
            if (removed > 0) view.RemoveRange(0, removed);
            for (next = Math.Max(next, log.First); next < log.Next; next++)
                if (Wanted(log[next])) view.Add(next);
            return removed;
        }

        public override int Count { get { return view.Count; } }

        LogLine Line(int row)
        {
            lock (session.Sync) return session.Log[view[row]];
        }

        static string Name(Level level)
        {
            return level == Level.Error ? "ERRO" : level == Level.Warn ? "AVISO"
                : level == Level.Info ? "info" : level == Level.Debug ? "medida" : "";
        }

        static Color Ink(Level level)
        {
            return level == Level.Error ? Look.BadInk : level == Level.Warn ? Look.Warn : level == Level.Info ? Look.Ink : Look.Soft;
        }

        /// The message, and who says it: the level has a column of its own.
        static string Body(LogLine line, out string from)
        {
            string text = line.Text;
            from = "";
            foreach (string word in new[] { "TRACE ", "DEBUG ", "INFO ", "WARN ", "ERROR " })
            {
                if (!text.StartsWith(word, StringComparison.Ordinal)) continue;
                int colon = text.IndexOf(": ", word.Length, StringComparison.Ordinal);
                if (colon < 0 || text.IndexOf(' ', word.Length, colon - word.Length) >= 0) return text.Substring(word.Length);
                from = text.Substring(word.Length, colon - word.Length).Replace("victory_hle::modules::", "").Replace("victory_", "");
                string said = text.Substring(colon + 2);
                // "dstorage: file opened" says who already.
                if (said.StartsWith(from + ":", StringComparison.Ordinal)) from = "";
                return said;
            }
            return text;
        }

        public override void Paint(Graphics g, int row, Rectangle r, bool selected)
        {
            LogLine line = Line(row);
            if (line == null) return;
            int x = r.X + Look.Px(10), time = Look.Px(62), level = Look.Px(52);
            Color ink = Ink(line.Level);
            Draw.Text(g, Say.Clock(line.At), Look.Mono, Look.Faint, new Rectangle(x, r.Y, time, r.Height));
            x += time + Look.Px(6);
            Draw.Text(g, Name(line.Level), Look.Mono, line.Level >= Level.Warn ? ink : Look.Faint, new Rectangle(x, r.Y, level, r.Height));
            x += level;
            string from, text = Body(line, out from);
            if (from != "")
            {
                int wide = Draw.Width(g, from, Look.Mono);
                Draw.Text(g, from, Look.Mono, Look.Faint, new Rectangle(x, r.Y, wide + 2, r.Height));
                x += wide + Look.Px(10);
            }
            // What no window is wide enough for is not measured either.
            if (text.Length > 600) text = text.Substring(0, 600);
            Draw.Text(g, text, Look.Mono, ink, new Rectangle(x, r.Y, r.Right - x - Look.Px(8), r.Height));
        }

        public override string Copy(int row)
        {
            LogLine line = Line(row);
            return line == null ? "" : Say.Clock(line.At) + "  " + line.Text;
        }
    }

    /// The panel beside the game.
    public sealed class PanelForm : Form
    {
        readonly string outputLog, errorLog;
        readonly Session session = new Session();
        readonly Reader reader;
        readonly Machine machine = new Machine();
        readonly Timer timer = new Timer();
        readonly Dashboard dash = new Dashboard();
        readonly RowList list = new RowList();
        readonly TextBox detail = new TextBox();
        readonly TextBox filter = new TextBox();
        readonly EventRows events;
        readonly LogRows log;
        readonly Chip summary = new Chip("Resumo", ChipRole.Choice), technical = new Chip("Técnico", ChipRole.Choice);
        readonly Chip copy = new Chip("Copiar resumo", ChipRole.Button), folder = new Chip("Abrir pasta de logs", ChipRole.Button);
        readonly Chip everything = new Chip("Tudo", ChipRole.Choice), warnings = new Chip("Avisos", ChipRole.Choice);
        readonly Chip errors = new Chip("Erros", ChipRole.Choice), follow = new Chip("Seguir", ChipRole.Toggle);
        readonly Chip noise = new Chip("Mostrar tudo", ChipRole.Toggle), pin = new Chip("Sempre visível", ChipRole.Toggle);
        Game game;
        Shot shot = new Shot();
        End end;
        int seen = -1;
        DateTime painted;
        bool laid;
        Rectangle head, bar, foot;
        string copied;
        DateTime copiedAt;

        /// Before any window: sizes of our own, on screens of any density.
        public static void Prepare()
        {
            try
            {
                Native.SetProcessDPIAware();
            }
            catch (EntryPointNotFoundException)
            {
            }
            Application.ThreadException += (sender, e) => Faults.Note("janela", e.Exception);
            using (Graphics g = Graphics.FromHwnd(IntPtr.Zero)) Look.Init(g.DpiX / 96);
        }

        public PanelForm(int pid, string outputLog, string errorLog)
        {
            this.outputLog = outputLog;
            this.errorLog = errorLog;
            if (Look.Body == null) Prepare();
            game = new Game(pid);
            if (game.Opened) session.Started = game.Started;
            events = new EventRows(session);
            log = new LogRows(session);
            reader = new Reader(session, outputLog, errorLog);
            Build();
        }

        /// The process watched: the title's, or the one it went on in.
        public int GamePid { get { return game.Pid; } }
        public Session Session { get { return session; } }
        public Shot Showing { get { return shot; } }

        /// What the script sampled; NaN where it could not.
        public void SetMachine(double systemCpu, double gameCpu, double threads, double processors, double ramUsed, double ramTotal, double gameRam)
        {
            machine.SystemCpu = systemCpu;
            machine.GameCpu = gameCpu;
            machine.Threads = threads;
            machine.Processors = processors;
            machine.RamUsed = ramUsed;
            machine.RamTotal = ramTotal;
            machine.GameRam = gameRam;
        }

        // Opening beside a game must not take the keyboard from it.
        protected override bool ShowWithoutActivation { get { return true; } }

        void Build()
        {
            SuspendLayout();
            Text = "Vyce | Painel";
            AutoScaleMode = AutoScaleMode.None;
            BackColor = Look.Back;
            ForeColor = Look.Ink;
            Font = Look.Body;
            KeyPreview = true;
            SetStyle(ControlStyles.OptimizedDoubleBuffer | ControlStyles.AllPaintingInWmPaint | ControlStyles.ResizeRedraw, true);
            Place();

            detail.Multiline = true;
            detail.ReadOnly = true;
            detail.BorderStyle = BorderStyle.None;
            detail.ScrollBars = ScrollBars.Vertical;
            detail.BackColor = Look.Card;
            detail.ForeColor = Look.Ink;
            detail.Font = Look.Body;
            detail.Visible = false;
            detail.AccessibleName = "Detalhe do evento";
            detail.HandleCreated += (sender, e) => Native.Darken(detail);
            filter.BorderStyle = BorderStyle.FixedSingle;
            filter.BackColor = Look.Card;
            filter.ForeColor = Look.Ink;
            filter.Font = Look.Body;
            filter.AccessibleName = "Filtrar o registro";
            filter.HandleCreated += (sender, e) => Native.SendMessage(filter.Handle, 0x1501, new IntPtr(1), "Filtrar, por exemplo: pipeline");
            list.Empty = "Nenhum evento ainda.";
            list.AccessibleName = "Eventos";

            ContextMenuStrip menu = new ContextMenuStrip();
            menu.Items.Add("Copiar", null, (sender, e) => CopyText(list.Copied(), "linhas copiadas"));
            menu.Items.Add("Abrir o arquivo de log", null, (sender, e) => Guard("abrir o log", () => Process.Start(errorLog)));
            list.ContextMenuStrip = menu;

            summary.Checked = everything.Checked = follow.Checked = true;
            warnings.Mark = Mark.Warn;
            errors.Mark = Mark.Error;
            summary.Click += (sender, e) => Mode(false);
            technical.Click += (sender, e) => Mode(true);
            copy.Click += (sender, e) => CopySummary();
            folder.Click += (sender, e) => Guard("abrir a pasta", OpenFolder);
            everything.Click += (sender, e) => Least(Mark.Info);
            warnings.Click += (sender, e) => Least(Mark.Warn);
            errors.Click += (sender, e) => Least(Mark.Error);
            follow.Click += (sender, e) => list.Follow = follow.Checked;
            noise.Click += (sender, e) => Filter();
            pin.Click += (sender, e) => TopMost = pin.Checked;
            filter.TextChanged += (sender, e) => Filter();
            list.FollowChanged += (sender, e) => follow.Checked = list.Follow;
            list.SelectionChanged += (sender, e) => Guard("detalhe", ShowDetail);
            Controls.AddRange(new Control[]
            {
                dash, list, detail, filter, summary, technical, copy, folder, everything, warnings, errors, follow, noise, pin,
            });
            list.Source = events;
            Mode(false);
            ResumeLayout(false);
            laid = true;
            Arrange();

            timer.Interval = 250;
            timer.Tick += (sender, e) => Guard("atualização", () => Tick(false));
        }

        /// Beside the game: at the right edge of the screen. VYCE_PANEL_BOUNDS
        /// ("x,y,width,height") puts it elsewhere.
        void Place()
        {
            Rectangle area = Screen.PrimaryScreen.WorkingArea;
            MinimumSize = new Size(Math.Min(Look.Px(860), area.Width), Math.Min(Look.Px(600), area.Height));
            StartPosition = FormStartPosition.Manual;
            int width = Math.Min(Look.Px(960), area.Width), height = Math.Min(Look.Px(1400), area.Height);
            Rectangle bounds = new Rectangle(area.Right - width, area.Top, width, height);
            string wanted = Environment.GetEnvironmentVariable("VYCE_PANEL_BOUNDS");
            string[] parts = (wanted ?? "").Split(',');
            int[] n = new int[4];
            bool good = parts.Length == 4;
            for (int k = 0; good && k < 4; k++) good = int.TryParse(parts[k].Trim(), out n[k]);
            if (good && n[2] > 0 && n[3] > 0) bounds = new Rectangle(n[0], n[1], n[2], n[3]);
            Bounds = bounds;
        }

        static void Guard(string what, Action act)
        {
            try
            {
                act();
            }
            catch (Exception e)
            {
                Faults.Note(what, e);
            }
        }

        protected override void OnHandleCreated(EventArgs e)
        {
            base.OnHandleCreated(e);
            Native.DarkenCaption(this);
        }

        protected override void OnShown(EventArgs e)
        {
            base.OnShown(e);
            reader.Start();
            timer.Start();
        }

        protected override void OnFormClosed(FormClosedEventArgs e)
        {
            timer.Stop();
            reader.Dispose();
            game.Dispose();
            base.OnFormClosed(e);
        }

        protected override void OnKeyDown(KeyEventArgs e)
        {
            if (e.Control && e.KeyCode == Keys.C && !filter.Focused && !detail.Focused)
            {
                string text = list.Copied();
                if (text != "")
                {
                    CopyText(text, "linhas copiadas");
                    e.Handled = true;
                }
            }
            if (e.Control && e.KeyCode == Keys.F)
            {
                Mode(true);
                filter.Focus();
                e.Handled = e.SuppressKeyPress = true;
            }
            if (e.KeyCode == Keys.Escape && filter.Focused)
            {
                filter.Text = "";
                e.Handled = e.SuppressKeyPress = true;
            }
            base.OnKeyDown(e);
        }

        // ---- layout ----

        protected override void OnResize(EventArgs e)
        {
            base.OnResize(e);
            if (laid && WindowState != FormWindowState.Minimized) Arrange();
        }

        int Put(Chip chip, int x, int y, int height, bool fromRight)
        {
            int width = chip.Wanted();
            if (fromRight) x -= width;
            chip.Bounds = new Rectangle(x, y, width, height);
            return fromRight ? x - Look.Px(6) : x + width + Look.Px(6);
        }

        void Arrange()
        {
            int pad = Look.Px(12), width = ClientSize.Width - 2 * pad, height = ClientSize.Height;
            int button = Look.Px(28);
            head = new Rectangle(pad, Look.Px(9), width, Look.Px(50));
            int right = pad + width;
            right = Put(folder, right, head.Y, button, true);
            right = Put(copy, right, head.Y, button, true) - Look.Px(8);
            right = Put(technical, right, head.Y, button, true) + Look.Px(4);
            right = Put(summary, right, head.Y, button, true);
            head.Width = width;
            stateWidth = right - head.X;

            int wanted = Dashboard.Wanted(height, technical.Checked);
            dash.Bounds = new Rectangle(pad, head.Bottom + Look.Px(4), width, wanted);
            dash.Arrange(height, technical.Checked);
            dash.Invalidate();

            bar = new Rectangle(pad, dash.Bottom + Look.Px(8), width, Look.Px(28));
            int left = bar.X + Look.Px(76);
            left = Put(everything, left, bar.Y, bar.Height, false);
            left = Put(warnings, left, bar.Y, bar.Height, false);
            left = Put(errors, left, bar.Y, bar.Height, false);
            right = Put(follow, pad + width, bar.Y, bar.Height, true);
            right = Put(noise, right, bar.Y, bar.Height, true);
            int from = bar.X + Look.Px(76);
            int wide = Math.Max(Look.Px(100), Math.Min(Look.Px(360), right - from - Look.Px(12)));
            filter.Bounds = new Rectangle(from, bar.Y + (bar.Height - filter.Height) / 2, wide, filter.Height);

            foot = new Rectangle(pad, height - Look.Px(25), width, Look.Px(20));
            Put(pin, pad + width, foot.Y - Look.Px(1), foot.Height + Look.Px(2), true);
            int y = bar.Bottom + Look.Px(5), bottom = foot.Y - Look.Px(4);
            int tall = 0;
            if (detail.Visible)
            {
                // As tall as its text, within reason.
                int most = Math.Max(Look.Px(84), (bottom - y) / 2);
                Size text = TextRenderer.MeasureText(detail.Text, detail.Font, new Size(width - Look.Px(46), int.MaxValue),
                    TextFormatFlags.WordBreak | TextFormatFlags.TextBoxControl | TextFormatFlags.NoPrefix);
                tall = Math.Max(Look.Px(60), Math.Min(most, text.Height + Look.Px(22)));
            }
            if (tall > 0) detail.Bounds = new Rectangle(pad + Look.Px(10), bottom - tall + Look.Px(6), width - Look.Px(12), tall - Look.Px(10));
            list.Bounds = new Rectangle(pad, y, width, Math.Max(Look.Px(40), bottom - y - (tall > 0 ? tall + Look.Px(2) : 0)));
            Invalidate();
        }

        int stateWidth;

        protected override void OnPaint(PaintEventArgs e)
        {
            Graphics g = e.Graphics;
            g.Clear(BackColor);
            Guard("cabeçalho", () => Head(g));
            Guard("rodapé", () => Foot(g));
        }

        void Head(Graphics g)
        {
            int line = Look.Px(28), x = head.X;
            Draw.Text(g, "Vyce", Look.Head, Look.Ink, new Rectangle(x, head.Y, stateWidth, line));
            x += Draw.Width(g, "Vyce", Look.Head) + Look.Px(16);
            int dot = Look.Px(10);
            Mark mark = shot.StateMark;
            if (mark == Mark.Good) Draw.Fill(g, new Rectangle(x, head.Y + (line - dot) / 2, dot, dot), 5, Look.Good);
            else Draw.Icon(g, mark, new Rectangle(x - Look.Px(1), head.Y + (line - Look.Px(13)) / 2, Look.Px(13), Look.Px(13)));
            x += Look.Px(17);
            string lasted = shot.Lasted == "" ? "" : "  ·  " + shot.Lasted;
            Draw.Text(g, shot.State + lasted, Look.Strong, mark == Mark.Error ? Look.BadInk : mark == Mark.Good ? Look.Ink : Look.Soft,
                new Rectangle(x, head.Y, head.X + stateWidth - x, line));
            Draw.Text(g, shot.Facts, Look.Small, Look.Faint, new Rectangle(head.X, head.Y + line + Look.Px(4), head.Width, Look.Px(16)));
        }

        void Foot(Graphics g)
        {
            if (detail.Visible)
                Draw.Fill(g, new Rectangle(list.Left, detail.Top - Look.Px(6), list.Width, detail.Height + Look.Px(10)), 5, Look.Card);
            Draw.Text(g, technical.Checked ? "Registro" : "Eventos", Look.Head, Look.Ink, new Rectangle(bar.X, bar.Y, Look.Px(76), bar.Height));
            string text = shot.Foot;
            Color ink = Look.Faint;
            if (copied != null && (DateTime.UtcNow - copiedAt).TotalSeconds < 4)
            {
                text = copied;
                ink = Look.Accent;
            }
            else if (Faults.Count > 0 && (DateTime.UtcNow - Faults.LastAt).TotalSeconds < 15)
            {
                text = "Uma parte do painel falhou (" + Faults.Count + "): " + Faults.Last;
                ink = Look.Warn;
            }
            else if (!reader.CaughtUp && reader.Share < 1) text = "Lendo o log: " + Say.Share(reader.Share);
            else if (Faults.Count > 0) text += "  ·  falhas do painel: " + Faults.Count;
            Draw.Text(g, text, Look.Small, ink, new Rectangle(foot.X, foot.Y, foot.Width - pin.Width - Look.Px(10), foot.Height));
        }

        // ---- what happens ----

        /// Looks at the game and at what was read. `now` forces a repaint.
        public void Tick(bool force)
        {
            DateTime now = DateTime.UtcNow;
            Guard("processo do jogo", () => Watch(now));
            bool changed = false;
            int removed = 0;
            lock (session.Sync)
            {
                changed = session.Version != seen;
                // The ages grow once a second though nothing is read.
                if (!changed && !force && (now - painted).TotalMilliseconds < 950) return;
                seen = session.Version;
                Guard("números", () => shot = Shot.Take(session, game.Pid, game.Alive, end, machine, now));
                if (changed || force) Guard("lista", () => removed = technical.Checked ? log.Update() : events.Update());
            }
            painted = now;
            if (changed || force) Guard("lista", () => list.Refill(removed));
            if (WindowState == FormWindowState.Minimized)
            {
                Caption();
                return;
            }
            Guard("números", () => dash.Shot = shot);
            Guard("números", Describe);
            Guard("contadores", Counts);
            Caption();
            Invalidate(Rectangle.FromLTRB(0, 0, ClientSize.Width, dash.Top));
            Invalidate(Rectangle.FromLTRB(0, foot.Y - Look.Px(2), ClientSize.Width, ClientSize.Height));
        }

        /// What a screen reader says of the numbers.
        void Describe()
        {
            string text = shot.Last == null ? shot.Note : Say.Fps(shot.Last.Fps) + " FPS, " + Say.Ms(shot.Last.Ms) + " por quadro, " + shot.AgeText;
            text += ". " + shot.Band.Title.Replace("  ·  ", " ") + ". " + shot.Verdict;
            if (dash.AccessibleDescription != text) dash.AccessibleDescription = text;
        }

        void Caption()
        {
            if (Text != shot.Title) Text = shot.Title;
        }

        void Counts()
        {
            string w = "Avisos" + (shot.Warnings > 0 ? " " + Say.Count(shot.Warnings) : "");
            string x = "Erros" + (shot.Errors > 0 ? " " + Say.Count(shot.Errors) : "");
            if (warnings.Text == w && errors.Text == x) return;
            warnings.Text = w;
            errors.Text = x;
            int left = bar.X + Look.Px(76);
            left = Put(everything, left, bar.Y, bar.Height, false);
            left = Put(warnings, left, bar.Y, bar.Height, false);
            Put(errors, left, bar.Y, bar.Height, false);
        }

        void Watch(DateTime now)
        {
            int next;
            lock (session.Sync) next = session.Relaunched;
            if (next > 0 && next != game.Pid && !game.Alive)
            {
                Game other = new Game(next);
                if (other.Alive)
                {
                    game.Dispose();
                    game = other;
                    end = null;
                }
                else other.Dispose();
            }
            if (game.Alive || end != null) return;
            // The log's last words come first.
            if (!reader.CaughtUp) return;
            uint code;
            bool known = game.ExitCode(out code);
            DateTime at = game.Exited;
            if (at != DateTime.MinValue && (now - at).TotalMilliseconds < 700) return;
            lock (session.Sync)
            {
                if (at == DateTime.MinValue) at = session.LastLineAt > DateTime.MinValue ? session.LastLineAt : now;
                end = End.Of(session, known, code, at);
                session.Tell(end.Bad ? Kinds.EndedBadly : Kinds.Ended, at, end.Whole);
            }
        }

        void Mode(bool deep)
        {
            summary.Checked = !deep;
            technical.Checked = deep;
            everything.Visible = warnings.Visible = errors.Visible = !deep;
            filter.Visible = noise.Visible = deep;
            list.Empty = deep ? "Nenhuma linha com esse filtro." : "Nenhum evento ainda.";
            list.AccessibleName = deep ? "Registro" : "Eventos";
            detail.Font = deep ? Look.Mono : Look.Body;
            lock (session.Sync)
            {
                if (deep) log.Filter(noise.Checked, filter.Text);
                if (deep) log.Update();
                else events.Update();
            }
            list.Source = deep ? (Rows)log : events;
            if (laid) Arrange();
        }

        void Filter()
        {
            lock (session.Sync)
            {
                log.Filter(noise.Checked, filter.Text);
                log.Update();
            }
            if (technical.Checked) list.Source = log;
        }

        void Least(Mark mark)
        {
            everything.Checked = mark == Mark.Info;
            warnings.Checked = mark == Mark.Warn;
            errors.Checked = mark == Mark.Error;
            events.Least = mark;
            list.Empty = mark == Mark.Error ? "Nenhum erro." : mark == Mark.Warn ? "Nenhum aviso." : "Nenhum evento ainda.";
            lock (session.Sync) events.Update();
            list.Source = events;
        }

        void ShowDetail()
        {
            bool some = list.Current >= 0;
            string text = some ? (technical.Checked ? list.Copied() : events.Detail(list.Current)) : "";
            if (some) detail.Text = text.Replace("\r\n", "\n").Replace("\n", "\r\n").TrimEnd();
            else if (!detail.Visible) return;
            detail.Visible = some;
            Arrange();
        }

        void CopyText(string text, string told)
        {
            if (string.IsNullOrEmpty(text)) return;
            Guard("copiar", () =>
            {
                // Another program may hold the clipboard for a moment.
                Clipboard.SetDataObject(text, true, 8, 60);
                copied = told;
                copiedAt = DateTime.UtcNow;
                Invalidate();
            });
        }

        public string SummaryText()
        {
            lock (session.Sync) return Vyce.Monitor.Summary.Text(session, shot, machine, errorLog);
        }

        void CopySummary()
        {
            Guard("resumo", () => Tick(true));
            string text = null;
            Guard("resumo", () => text = SummaryText());
            CopyText(text, "Resumo copiado: cole no chat.");
        }

        void OpenFolder()
        {
            if (File.Exists(errorLog)) Process.Start("explorer.exe", "/select,\"" + Path.GetFullPath(errorLog) + "\"");
            else
            {
                string dir = Path.GetDirectoryName(Path.GetFullPath(errorLog));
                if (Directory.Exists(dir)) Process.Start("explorer.exe", "\"" + dir + "\"");
            }
        }
    }
}
'@
