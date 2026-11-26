# The session panel's C#, first part: the logs, the events and the numbers.
# C# 5 (what Windows PowerShell compiles), kept in a script because the
# repository takes no .cs files. vyce-monitor.ps1 compiles both parts together.
$VyceMonitorModel = @'
namespace Vyce.Monitor
{
    using System;
    using System.Collections.Generic;
    using System.Globalization;
    using System.IO;
    using System.Runtime.InteropServices;
    using System.Text;
    using System.Threading;

    /// How an event looks.
    public enum Mark { Info, Good, Stage, Warn, Error }

    public enum Level : byte { Plain, Trace, Debug, Info, Warn, Error }

    /// Numbers and times as the panel writes them.
    public static class Say
    {
        public const string Nothing = "—";
        static readonly CultureInfo Pt = CultureInfo.GetCultureInfo("pt-BR");

        public static bool Known(double v)
        {
            return !double.IsNaN(v) && !double.IsInfinity(v);
        }

        public static string Number(double v, string format)
        {
            return Known(v) ? v.ToString(format, Pt) : Nothing;
        }

        public static string Count(double v)
        {
            return Number(Math.Round(v), "N0");
        }

        /// Three figures are enough: 214, 29,3, 4,3, 0,79.
        public static string Fps(double v)
        {
            if (!Known(v) || v < 0) return Nothing;
            return v.ToString(v >= 99.95 ? "N0" : v >= 0.995 ? "0.0" : "0.00", Pt);
        }

        public static string Ms(double v)
        {
            if (!Known(v) || v < 0) return Nothing;
            return v.ToString(v >= 99.95 ? "N0" : "0.0", Pt) + " ms";
        }

        public static string Share(double v)
        {
            return Known(v) ? (100 * v).ToString("0", Pt) + " %" : Nothing;
        }

        /// Of megabytes: 512 MB, 6,4 GB.
        public static string Size(double mb)
        {
            if (!Known(mb) || mb < 0) return Nothing;
            return mb >= 1000 ? (mb / 1024).ToString("0.0", Pt) + " GB" : mb.ToString("0", Pt) + " MB";
        }

        public static string Clock(DateTime utc)
        {
            return utc == DateTime.MinValue ? "" : utc.ToLocalTime().ToString("HH:mm:ss", Pt);
        }

        /// 3:25, 1:02:05.
        public static string Span(double seconds)
        {
            if (!Known(seconds)) return Nothing;
            long t = (long)Math.Max(0, Math.Floor(seconds));
            return t >= 3600
                ? string.Format(Pt, "{0}:{1:00}:{2:00}", t / 3600, t / 60 % 60, t % 60)
                : string.Format(Pt, "{0}:{1:00}", t / 60, t % 60);
        }

        /// 38 s, 4 min 09 s, 1 h 02 min.
        public static string Long(double seconds)
        {
            if (!Known(seconds)) return Nothing;
            long t = (long)Math.Max(0, Math.Round(seconds));
            if (t < 60) return t + " s";
            return t < 3600
                ? string.Format(Pt, "{0} min {1:00} s", t / 60, t % 60)
                : string.Format(Pt, "{0} h {1:00} min", t / 3600, t / 60 % 60);
        }

        /// An estimate: minutes from 90 s on, else tens of seconds.
        public static string About(double seconds)
        {
            if (!Known(seconds)) return "";
            return "faltam cerca de " + (seconds >= 90
                ? Math.Ceiling(seconds / 60).ToString("0", Pt) + " min"
                : Math.Max(10, 10 * Math.Ceiling(seconds / 10)).ToString("0", Pt) + " s");
        }

        public static string Ago(double seconds)
        {
            return "há " + Long(Math.Max(1, Math.Floor(seconds)));
        }

        public static string Cut(string text, int most)
        {
            return text.Length <= most ? text : text.Substring(0, most - 1) + "…";
        }
    }

    /// A small JSON reader: what the runtime's lines need.
    public sealed class Json
    {
        readonly string s;
        int i;
        int depth;

        Json(string text, int at)
        {
            s = text;
            i = at;
        }

        /// The object at `at`; null if it is broken or cut short.
        public static Dictionary<string, object> Read(string text, int at)
        {
            try
            {
                return new Json(text, at).Value() as Dictionary<string, object>;
            }
            catch (FormatException)
            {
                return null;
            }
        }

        char Peek()
        {
            while (i < s.Length && (s[i] == ' ' || s[i] == '\t' || s[i] == '\r' || s[i] == '\n')) i++;
            return i < s.Length ? s[i] : '\0';
        }

        void Take(char c)
        {
            if (Peek() != c) throw new FormatException();
            i++;
        }

        object Value()
        {
            if (++depth > 16) throw new FormatException();
            object value;
            char c = Peek();
            if (c == '{') value = Members();
            else if (c == '[') value = Items();
            else if (c == '"') value = Text();
            else if (Word("true")) value = true;
            else if (Word("false")) value = false;
            else if (Word("null")) value = null;
            else value = Number();
            depth--;
            return value;
        }

        bool Word(string word)
        {
            if (string.CompareOrdinal(s, i, word, 0, word.Length) != 0) return false;
            i += word.Length;
            return true;
        }

        Dictionary<string, object> Members()
        {
            Dictionary<string, object> members = new Dictionary<string, object>(StringComparer.Ordinal);
            Take('{');
            if (Peek() == '}')
            {
                i++;
                return members;
            }
            while (true)
            {
                if (Peek() != '"') throw new FormatException();
                string name = Text();
                Take(':');
                members[name] = Value();
                if (Peek() != ',') break;
                i++;
            }
            Take('}');
            return members;
        }

        List<object> Items()
        {
            List<object> items = new List<object>();
            Take('[');
            if (Peek() == ']')
            {
                i++;
                return items;
            }
            while (true)
            {
                items.Add(Value());
                if (Peek() != ',') break;
                i++;
            }
            Take(']');
            return items;
        }

        string Text()
        {
            StringBuilder text = new StringBuilder();
            i++;
            while (true)
            {
                if (i >= s.Length) throw new FormatException();
                char c = s[i++];
                if (c == '"') return text.ToString();
                if (c != '\\')
                {
                    text.Append(c);
                    continue;
                }
                if (i >= s.Length) throw new FormatException();
                c = s[i++];
                int code;
                if (c == 'u')
                {
                    if (i + 4 > s.Length || !int.TryParse(s.Substring(i, 4), NumberStyles.HexNumber,
                        CultureInfo.InvariantCulture, out code)) throw new FormatException();
                    text.Append((char)code);
                    i += 4;
                }
                else
                {
                    int at = "bfnrt".IndexOf(c);
                    text.Append(at < 0 ? c : "\b\f\n\r\t"[at]);
                }
            }
        }

        double Number()
        {
            int from = i;
            while (i < s.Length && "+-.eE0123456789".IndexOf(s[i]) >= 0) i++;
            double value;
            if (!double.TryParse(s.Substring(from, i - from), NumberStyles.Float,
                CultureInfo.InvariantCulture, out value)) throw new FormatException();
            return value;
        }
    }

    /// The numbers of one line of the runtime, by name.
    public sealed class Sample
    {
        static readonly Dictionary<string, int> Places = new Dictionary<string, int>(StringComparer.Ordinal);
        readonly double[] values;
        /// When the line was written (known, or reckoned from the frames).
        public DateTime At;
        /// Frames of the interval it covers.
        public double Frames = 1;

        public Sample(Dictionary<string, object> fields)
        {
            lock (Places)
            {
                foreach (string name in fields.Keys)
                    if (!Places.ContainsKey(name) && Places.Count < 400) Places[name] = Places.Count;
                values = new double[Places.Count];
                for (int k = 0; k < values.Length; k++) values[k] = double.NaN;
                foreach (KeyValuePair<string, object> f in fields)
                {
                    int place;
                    if (!Places.TryGetValue(f.Key, out place)) continue;
                    if (f.Value is double) values[place] = (double)f.Value;
                    else if (f.Value is bool) values[place] = (bool)f.Value ? 1 : 0;
                }
            }
        }

        /// Every name a line had so far.
        public static string[] Names()
        {
            lock (Places)
            {
                string[] names = new string[Places.Count];
                foreach (KeyValuePair<string, int> place in Places) names[place.Value] = place.Key;
                return names;
            }
        }

        /// NaN if the line has no such number.
        public double this[string name]
        {
            get
            {
                int place;
                lock (Places) if (!Places.TryGetValue(name, out place)) return double.NaN;
                return place < values.Length ? values[place] : double.NaN;
            }
        }

        public double Fps { get { return this["fps"]; } }
        public double Ms { get { return this["frame_ms"]; } }
        public double Total { get { return this["frames_total"]; } }
        public double Draws { get { return this["draws_per_frame"]; } }
        public double Dropped { get { return this["dropped_per_frame"]; } }

        /// Indirect executions dropped; older builds name it otherwise.
        public double Indirect
        {
            get
            {
                double now = this["indirect_dropped_per_frame"];
                return Say.Known(now) ? now : this["indirect_per_frame"];
            }
        }

        public double Making { get { return this["pipelines_building"]; } }

        public static double OrZero(double v)
        {
            return Say.Known(v) ? v : 0;
        }
    }

    /// A VYCE_LOADING line.
    public sealed class LoadReport
    {
        readonly Dictionary<string, object> fields;
        public readonly DateTime At;

        public LoadReport(Dictionary<string, object> fields, DateTime at)
        {
            this.fields = fields;
            At = at;
        }

        public double this[string name]
        {
            get
            {
                object v;
                return fields.TryGetValue(name, out v) && v is double ? (double)v : double.NaN;
            }
        }

        public string Stage
        {
            get
            {
                object v;
                return fields.TryGetValue("stage", out v) && v is string ? (string)v : "";
            }
        }

        public bool Flag(string name)
        {
            object v;
            return fields.TryGetValue(name, out v) && v is bool && (bool)v;
        }
    }

    /// A line of the logs, as the technical view shows it.
    public sealed class LogLine
    {
        public DateTime At;
        public Level Level;
        /// Hidden unless everything is asked for.
        public bool Noise;
        public string Text;
    }

    /// Takes lines of the runtime apart.
    public static class Lines
    {
        /// Without terminal codes, tabs and other control characters.
        public static string Clean(string s)
        {
            int k = 0;
            while (k < s.Length && s[k] >= ' ' && s[k] != '﻿') k++;
            if (k == s.Length) return s;
            StringBuilder text = new StringBuilder(s.Length);
            text.Append(s, 0, k);
            while (k < s.Length)
            {
                char c = s[k++];
                if (c == '\x1b')
                {
                    if (k < s.Length && s[k] == '[')
                    {
                        k++;
                        while (k < s.Length && (s[k] < '@' || s[k] > '~')) k++;
                    }
                    k++;
                }
                else if (c == '\t') text.Append(' ');
                else if (c >= ' ' && c != '﻿') text.Append(c);
            }
            return text.ToString();
        }

        static int Digits(string s, int at, int count)
        {
            int n = 0;
            for (int k = at; k < at + count; k++)
            {
                if (s[k] < '0' || s[k] > '9') return -1;
                n = 10 * n + s[k] - '0';
            }
            return n;
        }

        /// The time a line starts with; where it ends, or 0 if there is none.
        public static int Stamp(string s, out DateTime utc)
        {
            utc = DateTime.MinValue;
            if (s.Length < 20 || s[4] != '-' || s[7] != '-' || s[10] != 'T' || s[13] != ':' || s[16] != ':') return 0;
            int year = Digits(s, 0, 4), month = Digits(s, 5, 2), day = Digits(s, 8, 2);
            int hour = Digits(s, 11, 2), minute = Digits(s, 14, 2), second = Digits(s, 17, 2);
            int at = 19;
            long ticks = 0;
            if (s[at] == '.')
            {
                long scale = 1000000;
                for (at++; at < s.Length && s[at] >= '0' && s[at] <= '9'; at++)
                {
                    ticks += (s[at] - '0') * scale;
                    scale /= 10;
                }
            }
            if (at >= s.Length || s[at] != 'Z') return 0;
            if (year < 2000 || month < 1 || month > 12 || day < 1 || day > 31 || hour < 0 || hour > 23
                || minute < 0 || minute > 59 || second < 0 || second > 59) return 0;
            utc = new DateTime(year, month, 1, hour, minute, second, DateTimeKind.Utc).AddDays(day - 1).AddTicks(ticks);
            return at + 1;
        }

        static readonly string[] Words = { "", "TRACE", "DEBUG", "INFO", "WARN", "ERROR" };

        /// The level, target and message of a cleaned line. A line that is
        /// not the runtime's log has no target.
        public static Level Parts(string text, int at, out string target, out string body)
        {
            target = null;
            body = text;
            if (at == 0) return Bare(text);
            while (at < text.Length && text[at] == ' ') at++;
            for (int k = 1; k < Words.Length; k++)
            {
                string word = Words[k];
                if (string.CompareOrdinal(text, at, word, 0, word.Length) != 0) continue;
                int from = at + word.Length;
                while (from < text.Length && text[from] == ' ') from++;
                int colon = text.IndexOf(": ", from, StringComparison.Ordinal);
                if (colon < 0 || text.IndexOf(' ', from, colon - from) >= 0)
                {
                    target = "";
                    body = text.Substring(from);
                }
                else
                {
                    target = text.Substring(from, colon - from);
                    body = text.Substring(colon + 2);
                }
                return (Level)k;
            }
            body = text.Substring(at);
            return Level.Plain;
        }

        /// The level of a line that only starts with the word for it.
        static Level Bare(string text)
        {
            int at = 0;
            while (at < text.Length && (text[at] == ' ' || text[at] == '[')) at++;
            if (Begins(text, at, "error") || Begins(text, at, "fatal") || Begins(text, at, "panic")) return Level.Error;
            return Begins(text, at, "warning") || Begins(text, at, "warn") ? Level.Warn : Level.Plain;
        }

        static bool Begins(string text, int at, string word)
        {
            if (string.Compare(text, at, word, 0, word.Length, StringComparison.OrdinalIgnoreCase) != 0) return false;
            int end = at + word.Length;
            return end >= text.Length || text[end] == ':' || text[end] == ' ' || text[end] == ']';
        }

        static bool IsName(char c)
        {
            return c >= 'a' && c <= 'z' || c == '_' || c >= '0' && c <= '9';
        }

        /// Where the next ` name=` starts; the end if there is none.
        static int NextField(string body, int from)
        {
            for (int space = body.IndexOf(' ', from); space >= 0; space = body.IndexOf(' ', space + 1))
            {
                int k = space + 1;
                while (k < body.Length && IsName(body[k])) k++;
                if (k > space + 1 && k < body.Length && body[k] == '=') return space;
            }
            return body.Length;
        }

        /// The message without its fields.
        public static string Message(string body)
        {
            return body.Substring(0, NextField(body, 0)).Trim();
        }

        /// The value of a field; null if the line has no such field.
        public static string Field(string body, string name)
        {
            for (int at = body.IndexOf(name, StringComparison.Ordinal); at >= 0;
                at = body.IndexOf(name, at + 1, StringComparison.Ordinal))
            {
                int eq = at + name.Length;
                if (eq >= body.Length || body[eq] != '=' || at > 0 && body[at - 1] != ' ') continue;
                int from = eq + 1;
                if (from >= body.Length || body[from] != '"') return body.Substring(from, NextField(body, from) - from).Trim();
                StringBuilder text = new StringBuilder();
                for (int k = from + 1; k < body.Length && body[k] != '"'; k++)
                {
                    if (body[k] == '\\' && k + 1 < body.Length) k++;
                    text.Append(body[k]);
                }
                return text.ToString();
            }
            return null;
        }

        public static double Value(string body, string name)
        {
            string text = Field(body, name);
            double value;
            return text != null && double.TryParse(text, NumberStyles.Float, CultureInfo.InvariantCulture, out value)
                ? value : double.NaN;
        }

        /// The first number after `word`.
        public static double After(string text, string word)
        {
            int at = text.IndexOf(word, StringComparison.Ordinal);
            if (at < 0) return double.NaN;
            at += word.Length;
            int end = at;
            while (end < text.Length && (text[end] >= '0' && text[end] <= '9' || text[end] == '.')) end++;
            double value;
            return double.TryParse(text.Substring(at, end - at), NumberStyles.Float,
                CultureInfo.InvariantCulture, out value) ? value : double.NaN;
        }

        /// A message with its numbers left out: what is said, not of what.
        public static string Shape(string message)
        {
            StringBuilder shape = new StringBuilder();
            foreach (char c in message)
            {
                if (c < '0' || c > '9') shape.Append(c);
                else if (shape.Length == 0 || shape[shape.Length - 1] != '#') shape.Append('#');
            }
            return Say.Cut(shape.ToString(), 120);
        }

        public static string FileName(string path)
        {
            int at = path.LastIndexOfAny(new[] { '\\', '/' });
            return at < 0 ? path : path.Substring(at + 1);
        }
    }

    /// What an event is and how it is told.
    public sealed class Kind
    {
        public readonly string Id;
        public readonly Mark Mark;
        /// In the summary's counts.
        public readonly string Label;
        public readonly Func<Row, string> Text;
        /// What it means, for the detail pane.
        public readonly string Why;
        /// Seconds without one more after which the next starts another row.
        public readonly double Gap;

        public Kind(string id, Mark mark, string label, Func<Row, string> text, string why = null, double gap = 20)
        {
            Id = id;
            Mark = mark;
            Label = label;
            Text = text;
            Why = why;
            Gap = gap;
        }
    }

    /// One row of the event list: events of a kind in a row.
    public sealed class Row
    {
        const int KeptFirst = 4, KeptLast = 8;
        public readonly Kind Kind;
        public DateTime First;
        public DateTime Last;
        public int Count;
        public string Note;
        /// Counts and sizes the text tells, by kind.
        public double A = double.NaN, B = double.NaN, C = double.NaN;
        /// Lines of the log it comes from: the first and the last ones.
        public readonly List<string> Kept = new List<string>();
        public int Left;

        public Row(Kind kind, DateTime at)
        {
            Kind = kind;
            First = Last = at;
        }

        public void Keep(string line)
        {
            if (line == null || Kept.Contains(line)) return;
            if (Kept.Count >= KeptFirst + KeptLast)
            {
                Kept.RemoveAt(KeptFirst);
                Left++;
            }
            Kept.Add(line);
        }

        /// What follows a report in the log (registers, frames).
        public void Follow(string line)
        {
            if (Kept.Count < 120) Kept.Add(line);
        }

        public void Most(double v)
        {
            if (Say.Known(v) && !(A >= v)) A = v;
        }

        public string Text()
        {
            string prefix = Kind.Mark == Mark.Warn ? "Aviso: " : Kind.Mark == Mark.Error ? "ERRO: " : "";
            return prefix + Kind.Text(this);
        }
    }

    /// The events, and how many of each kind there were.
    public sealed class Journal
    {
        const int MostRows = 1500;
        public readonly List<Row> Rows = new List<Row>();
        readonly Dictionary<string, Row> open = new Dictionary<string, Row>(StringComparer.Ordinal);
        public readonly Dictionary<string, Total> Totals = new Dictionary<string, Total>(StringComparer.Ordinal);
        /// Rows let go of at the front so far.
        public int Removed;
        public int Warnings, Errors;

        public sealed class Total
        {
            public Kind Kind;
            public int Count;
            public Row Last;
        }

        /// Whatever comes next starts rows of its own.
        public void Close()
        {
            open.Clear();
        }

        /// The row of a kind that is still going on, if any.
        public Row Peek(Kind kind, DateTime at)
        {
            Row row;
            return open.TryGetValue(kind.Id, out row) && (at - row.Last).TotalSeconds <= kind.Gap ? row : null;
        }

        /// The row the next event of a kind goes to, not counted yet.
        public Row Open(Kind kind, DateTime at)
        {
            Row row = Peek(kind, at);
            if (row == null)
            {
                row = new Row(kind, at);
                Rows.Add(row);
                open[kind.Id] = row;
                if (Rows.Count > MostRows) Shorten();
            }
            return row;
        }

        public Row Put(Kind kind, DateTime at, string line)
        {
            Row row = Open(kind, at);
            row.Count++;
            if (at > row.Last) row.Last = at;
            row.Keep(line);
            Total total;
            if (!Totals.TryGetValue(kind.Id, out total)) Totals[kind.Id] = total = new Total { Kind = kind };
            total.Count++;
            total.Last = row;
            if (kind.Mark == Mark.Warn) Warnings++;
            if (kind.Mark == Mark.Error) Errors++;
            return row;
        }

        /// Lets go of the oldest rows, those that only inform first.
        void Shorten()
        {
            List<Row> kept = new List<Row>(Rows.Count);
            int go = MostRows / 4;
            foreach (Row row in Rows)
            {
                if (go > 0 && row.Kind.Mark <= Mark.Good) go--;
                else kept.Add(row);
            }
            if (go > 0) kept.RemoveRange(0, Math.Min(go, kept.Count));
            Removed += Rows.Count - kept.Count;
            Rows.Clear();
            Rows.AddRange(kept);
            // What goes on goes on in a row that is seen.
            HashSet<Row> seen = new HashSet<Row>(kept);
            foreach (string id in new List<string>(open.Keys))
                if (!seen.Contains(open[id])) open.Remove(id);
        }

        public int CountOf(string id)
        {
            Total total;
            return Totals.TryGetValue(id, out total) ? total.Count : 0;
        }
    }

    /// The last lines of the logs.
    public sealed class Ring
    {
        const int Most = 20000;
        const long MostChars = 6000000;
        readonly LogLine[] lines = new LogLine[Most];
        long chars;
        public long First, Next;

        public void Add(LogLine line)
        {
            if (line.Text.Length > 4000) line.Text = Say.Cut(line.Text, 4000);
            while (Next - First >= Most || chars > MostChars && Next > First)
            {
                chars -= lines[First % Most].Text.Length;
                lines[First++ % Most] = null;
            }
            lines[Next++ % Most] = line;
            chars += line.Text.Length;
        }

        public LogLine this[long number]
        {
            get { return number < First || number >= Next ? null : lines[number % Most]; }
        }
    }

    /// The stage of the run from the frames alone, for builds that do not
    /// tell it: the runtime's own rules.
    public sealed class Guess
    {
        public string Stage = "";
        bool been;
        string look = "";
        int alike;
        DateTime lookAt;

        static string Look(double fps, double work)
        {
            if (fps >= 20 && fps <= 34 && work >= 5 && work <= 30) return "load";
            return fps >= 45 && work >= 9 && work <= 100 ? "menu" : "";
        }

        /// True if the stage changed; `since` is when it began.
        public bool Add(Sample s, out DateTime since)
        {
            since = s.At;
            string before = Stage;
            double work = Sample.OrZero(s.Draws) + Sample.OrZero(s.Dropped) + Sample.OrZero(s.Indirect);
            if (Stage == "") Stage = "start";
            if (work >= 150 && work * Math.Max(1, s.Frames) >= 300)
            {
                Stage = "world";
                been = true;
                alike = 0;
                return Stage != before;
            }
            string now = Look(s.Fps, work);
            if (now != look || alike == 0)
            {
                look = now;
                alike = 0;
                lookAt = s.At;
            }
            alike++;
            if (Stage == "start")
            {
                if (alike >= 2 && now != "") Stage = now == "load" ? "load" : "idle";
            }
            else if (Stage == "load" || Stage == "again")
            {
                if (now == "menu" && alike >= 4) Stage = "idle";
            }
            else if (now == "load" && alike >= (Stage == "world" ? 4 : 2)) Stage = been ? "again" : "load";
            if (Stage == before) return false;
            if (Stage != "start") since = lookAt;
            return true;
        }
    }

    /// Where the time of a frame goes, over a few seconds.
    public sealed class Parts
    {
        public double Ms, Gpu, Prepare, Back, Rest, Busy, PipelineWait;

        public static Parts Of(IList<Sample> samples)
        {
            Parts p = new Parts();
            double frames = 0;
            bool busy = true;
            foreach (Sample s in samples)
            {
                double n = Math.Max(1, s.Frames);
                frames += n;
                p.Ms += n * s.Ms;
                p.Gpu += n * Sample.OrZero(s["gpu_wait_ms"]);
                p.Prepare += n * Sample.OrZero(s["cpu_prepare_ms"]);
                p.Back += n * Sample.OrZero(s["copy_back_ms"]);
                p.PipelineWait += n * Sample.OrZero(s["pipeline_wait_ms"]);
                busy &= Say.Known(s["gpu_busy_ms"]);
                p.Busy += n * s["gpu_busy_ms"];
            }
            if (frames == 0 || !(p.Ms > 0)) return null;
            p.Ms /= frames;
            p.Gpu /= frames;
            p.Prepare /= frames;
            p.Back /= frames;
            p.PipelineWait /= frames;
            p.Busy = busy ? p.Busy / frames : double.NaN;
            // Parts that overlap can add up to more than the frame.
            double sum = p.Gpu + p.Prepare + p.Back;
            if (sum > p.Ms)
            {
                p.Gpu *= p.Ms / sum;
                p.Prepare *= p.Ms / sum;
                p.Back *= p.Ms / sum;
                sum = p.Ms;
            }
            p.Rest = p.Ms - sum;
            return p;
        }

        /// What holds the frame up, in a sentence.
        public string Verdict()
        {
            double fps = 1000 / Ms;
            double work = (Gpu + Prepare + Back) / Ms;
            // With work given ahead, the GPU is busy while the CPU prepares.
            bool ahead = Busy > 1.15 * Gpu;
            double gpu = (ahead ? Math.Min(Ms, Busy) : Gpu) / Ms;
            if (fps >= 25 && work < 0.5 && gpu < 0.5)
                return "Há folga: a GPU e o Vyce usam só " + Say.Share(Math.Max(work, gpu)) + " do quadro";
            double prepare = Prepare / Ms, back = Back / Ms, rest = Rest / Ms;
            if (gpu >= prepare && gpu >= back && gpu >= rest)
                return (ahead ? "A GPU trabalha em " : "A GPU segura ") + Say.Share(gpu) + " do quadro";
            if (prepare >= back && prepare >= rest) return "O preparo na CPU segura " + Say.Share(prepare) + " do quadro";
            if (back >= rest) return "A cópia de volta segura " + Say.Share(back) + " do quadro";
            if (PipelineWait > 0.3 * Ms && Rest > 0.3 * Ms)
                return "O quadro espera pipelines ficarem prontos (" + Say.Share(Math.Min(1, PipelineWait / Ms)) + ")";
            return "O próprio jogo e as esperas ocupam " + Say.Share(rest) + " do quadro";
        }
    }

    /// What the loading band shows.
    public sealed class Band
    {
        public string Title = "";
        public string Right = "";
        public string Detail = "";
        /// Of the bar; NaN for none.
        public double Share = double.NaN;
        public Mark Mark = Mark.Stage;
    }

    /// Memory and processors, sampled by the script.
    public sealed class Machine
    {
        public double SystemCpu = double.NaN, GameCpu = double.NaN, Threads = double.NaN, Processors = double.NaN;
        public double RamUsed = double.NaN, RamTotal = double.NaN, GameRam = double.NaN;
    }
}
namespace Vyce.Monitor
{
    using System;
    using System.Collections.Generic;

    /// Exit and exception codes, in words.
    public static class Codes
    {
        public static string Name(uint code)
        {
            switch (code)
            {
                case 0xC0000005: return "acesso inválido à memória";
                case 0xC0000409: return "verificação de segurança falhou (muitas vezes, dentro do driver de vídeo)";
                case 0xC0000374: return "memória corrompida";
                case 0x80000003: return "o jogo parou em uma verificação interna dele";
                case 0xC000001D: return "instrução que este processador não tem";
                case 0xC0000096: return "instrução não permitida";
                case 0xC00000FD: return "pilha estourada";
                case 0xC0000094: return "divisão por zero";
                case 0xE06D7363: return "exceção C++ sem tratamento";
                case 0xC000013A: return "interrompido (Ctrl+C ou console fechado)";
                case 0xC0000017:
                case 0xC000012D: return "acabou a memória do Windows";
                case 0x887A0005: return "a placa de vídeo parou de responder (device removed)";
                case 0x887A0006: return "a placa de vídeo travou (device hung)";
                case 0x887A0007: return "a placa de vídeo foi reiniciada (device reset)";
                case 0x40000015:
                case 3: return "encerrado por um erro fatal (abort)";
                case 101: return "falha interna do Vyce (panic)";
                case 1: return "erro ao iniciar, ou encerrado pelo Gerenciador de Tarefas";
                default: return null;
            }
        }

        public static string Hex(uint code)
        {
            return code < 0x10000 ? code.ToString() : "0x" + code.ToString("X8");
        }
    }

    /// Every kind of event the list tells. The texts are for someone who
    /// plays, not for someone who programs; the log's own lines follow in
    /// the detail pane.
    public static class Kinds
    {
        static string N(Row r)
        {
            return Say.Count(r.Count);
        }

        /// One event's text, or the count and the text of many.
        static string Of(Row r, string one, string many)
        {
            return r.Count == 1 ? one : N(r) + " " + many;
        }

        static string Bytes(double bytes)
        {
            return Say.Size(bytes / 1048576);
        }

        const string Drops = "O que usa isso não aparece na imagem. O jogo continua.";

        // The run.
        public static readonly Kind Build = new Kind("build", Mark.Stage, "versão do jogo",
            r => "Versão do jogo: " + r.Note);
        public static readonly Kind BuildUnknown = new Kind("build-unknown", Mark.Warn, "versão do jogo desconhecida",
            r => "versão do jogo desconhecida", "O Vyce foi testado com outras versões: pode não funcionar.");
        public static readonly Kind Console = new Kind("console", Mark.Info, "console",
            r => "Console simulado: " + r.Note);
        public static readonly Kind Mapped = new Kind("mapped", Mark.Info, "jogo na memória",
            r => "Jogo carregado na memória");
        public static readonly Kind Library = new Kind("title-dll", Mark.Info, "bibliotecas do jogo",
            r => Of(r, "Biblioteca do jogo carregada: " + r.Note, "bibliotecas do jogo carregadas"));
        public static readonly Kind Bound = new Kind("bound", Mark.Info, "funções ligadas",
            r => "Funções do console ligadas, nenhuma faltando");
        public static readonly Kind Missing = new Kind("missing", Mark.Warn, "funções do console que faltam",
            r => Of(r, "falta uma função do console: " + r.Note, "funções do console faltam (a última: " + r.Note + ")"),
            "Se o jogo chamar uma delas, ele para.");
        public static readonly Kind Files = new Kind("file", Mark.Info, "arquivos do jogo abertos",
            r => r.Count == 1 ? "Arquivo do jogo aberto: " + r.Note : "Arquivos do jogo abertos: " + N(r) + " (o último: " + r.Note + ")");
        public static readonly Kind FileFailed = new Kind("file-failed", Mark.Error, "arquivos do jogo que não abriram",
            r => Of(r, "um arquivo do jogo não abriu: " + r.Note, "arquivos do jogo não abriram (o último: " + r.Note + ")"),
            "Confira se os arquivos do jogo estão completos e se o disco responde.");
        public static readonly Kind ReadFailed = new Kind("read-failed", Mark.Error, "leituras do jogo que falharam",
            r => Of(r, "uma leitura do jogo falhou: " + r.Note, "leituras do jogo falharam (a última: " + r.Note + ")"),
            "Uma parte do jogo não carregou. Confira os arquivos e o disco.");
        public static readonly Kind Pad = new Kind("pad", Mark.Info, "controles",
            r => r.Note);
        public static readonly Kind Sound = new Kind("sound", Mark.Info, "áudio",
            r => "Áudio do jogo pronto");
        public static readonly Kind Net = new Kind("net", Mark.Info, "rede",
            r => Of(r, "O jogo tentou usar a rede; foi bloqueado", "vezes o jogo tentou usar a rede; foi bloqueado"),
            "O jogo roda sem internet. Nada foi enviado.");
        public static readonly Kind Said = new Kind("said", Mark.Info, "mensagens do jogo",
            r => Of(r, "O jogo avisou: " + r.Note, "mensagens do jogo (a última: " + r.Note + ")"));
        public static readonly Kind Tuned = new Kind("tuned", Mark.Info, "ajustes",
            r => "Ajuste ligado: " + r.Note);

        // Stages.
        public static readonly Kind Start = new Kind("stage-start", Mark.Stage, "etapas", r => "Iniciando o jogo", null, 0);
        public static readonly Kind Idle = new Kind("stage-idle", Mark.Stage, "etapas", r => "No menu ou em um vídeo", null, 0);
        public static readonly Kind Load = new Kind("stage-load", Mark.Stage, "etapas", r => "Carregando o mundo", null, 0);
        public static readonly Kind Again = new Kind("stage-again", Mark.Stage, "etapas", r => "Carregando outro lugar", null, 0);
        public static readonly Kind World = new Kind("stage-world", Mark.Stage, "etapas",
            r => "No mundo" + (Say.Known(r.A) ? " (carregou em " + Say.Long(r.A) + ")" : ""), null, 0);
        public static readonly Kind LoadDone = new Kind("load-done", Mark.Good, "carregamentos",
            r => r.Note + " em " + Say.Long(r.A) + (r.B > 0 ? ", " + Say.Size(r.B) + " lidos do disco" : ""), null, 0);
        public static readonly Kind LoadLost = new Kind("load-lost", Mark.Warn, "progresso do carregamento",
            r => "o progresso do carregamento não pôde ser lido ou guardado (" + r.Note + ")");
        public static readonly Kind Closing = new Kind("closing", Mark.Stage, "encerramento",
            r => "Janela do jogo fechada; encerrando");
        public static readonly Kind Ending = new Kind("ending", Mark.Info, "encerramento",
            r => r.Note, null, 0);
        public static readonly Kind Ended = new Kind("ended", Mark.Stage, "fim", r => "Jogo encerrado: " + r.Note, null, 0);
        public static readonly Kind EndedBadly = new Kind("ended-badly", Mark.Error, "fim com erro", r => "jogo encerrado: " + r.Note,
            "Use Copiar resumo para mandar o que aconteceu.", 0);
        public static readonly Kind Relaunch = new Kind("relaunch", Mark.Stage, "reinícios",
            r => "O jogo reiniciou em outro processo (" + r.Note + ")", null, 0);
        public static readonly Kind Exits = new Kind("exits", Mark.Info, "saídas pedidas pelo jogo",
            r => "O jogo pediu para sair (código " + r.Note + ")");

        // Picture.
        public static readonly Kind Adapter = new Kind("adapter", Mark.Good, "placa de vídeo",
            r => "Placa de vídeo: " + r.Note);
        public static readonly Kind Software = new Kind("software", Mark.Warn, "sem placa de vídeo",
            r => "desenhando sem a placa de vídeo (WARP), muito lento");
        public static readonly Kind Device = new Kind("device", Mark.Good, "vídeo",
            r => "Vídeo pronto (Direct3D 12)");
        public static readonly Kind NoDevice = new Kind("no-device", Mark.Error, "sem Direct3D 12",
            r => "o Direct3D 12 da placa de vídeo não abriu; nada será desenhado", "Atualize o driver de vídeo.");
        public static readonly Kind Bar = new Kind("rebar", Mark.Info, "Resizable BAR",
            r => "Resizable BAR: " + r.Note);
        public static readonly Kind Window = new Kind("window", Mark.Info, "janela",
            r => "Janela do jogo: " + r.Note);
        public static readonly Kind Picture = new Kind("picture", Mark.Info, "imagem",
            r => "Imagem: " + r.Note);
        public static readonly Kind PicturePath = new Kind("picture-path", Mark.Info, "caminho da imagem",
            r => "Imagem mostrada por " + r.Note, null, 0);
        public static readonly Kind FirstFrame = new Kind("first-frame", Mark.Good, "primeiro quadro",
            r => "Primeiro quadro na tela");
        public static readonly Kind NoPicture = new Kind("no-picture", Mark.Error, "imagem não mostrada",
            r => Of(r, "a imagem não pôde ser mostrada: " + r.Note, "vezes a imagem não pôde ser mostrada: " + r.Note));
        public static readonly Kind VideoMemory = new Kind("video-memory", Mark.Info, "memória de vídeo",
            r => "Memória de vídeo: " + r.Note + (r.Count > 1 ? " (" + N(r) + " medidas)" : ""),
            "Medida pelo Vyce depois que o jogo cria as imagens dele, e depois a cada minuto.", 90);
        public static readonly Kind GpuTop = new Kind("gpu-top", Mark.Info, "GPU",
            r => "O que mais pesa na GPU: " + r.Note, null, double.MaxValue);

        // Shaders and pipelines.
        const string WhatShader = "Shader é o programa que pinta um material ou um efeito. ";
        const string WhatPipeline = "Pipeline é um shader já pronto para a placa usar. ";
        public static readonly Kind Shader = new Kind("shader", Mark.Info, "shaders traduzidos",
            r => "Shaders traduzidos: " + N(r), WhatShader + "Os que já estavam no cache contam aqui e custam pouco.", 120);
        public static readonly Kind Pipeline = new Kind("pipeline", Mark.Info, "pipelines criados",
            r => "Pipelines criados: " + N(r) + (r.B > 0 ? " (" + Say.Count(r.B) + (r.B == 1 ? " recusado)" : " recusados)") : ""),
            WhatPipeline + "Enquanto faltam, os objetos que os usam não aparecem.", 120);
        public static readonly Kind Refused = new Kind("refused", Mark.Warn, "pipelines recusados pelo driver",
            r => Of(r, "1 pipeline recusado pelo driver (" + r.Note + ")", "pipelines recusados pelo driver (" + r.Note + ")"),
            WhatPipeline + "O driver não aceitou estes. " + Drops);
        public static readonly Kind NoPipeline = new Kind("no-pipeline", Mark.Warn, "pipelines não feitos",
            r => Of(r, "pipeline não feito: " + r.Note, "pipelines não feitos (" + r.Note + ")"), Drops);
        public static readonly Kind Crashed = new Kind("crashed", Mark.Error, "falhas do compilador do driver",
            r => Of(r, "o compilador do driver falhou em um pipeline (ele fica de fora)",
                "pipelines em que o compilador do driver falhou (ficam de fora)"),
            WhatPipeline + "O compilador do driver de vídeo quebrou ao fazer estes. " + Drops
                + " A lista fica guardada por driver e eles não são tentados de novo.");
        public static readonly Kind Endless = new Kind("endless", Mark.Error, "pipelines que o driver não termina",
            r => Of(r, "o compilador do driver não termina um pipeline (fica de fora na próxima vez)",
                "pipelines que o compilador do driver não termina (ficam de fora na próxima vez)"), Drops);
        public static readonly Kind LeftOut = new Kind("left-out", Mark.Warn, "pipelines deixados de fora",
            r => Say.Count(r.A) + " pipelines deixados de fora: o driver já falhou neles antes", Drops);
        public static readonly Kind LibraryLoaded = new Kind("library-loaded", Mark.Info, "pipelines lidos do disco",
            r => "Pipelines guardados lidos do disco: " + Bytes(r.A));
        public static readonly Kind LibraryRejected = new Kind("library-rejected", Mark.Warn, "pipelines guardados que não servem",
            r => "os pipelines guardados não servem para este driver; serão feitos de novo",
            "Acontece depois de trocar de driver ou de placa. A partida fica mais lenta até tudo ser feito outra vez.");
        public static readonly Kind LibraryNew = new Kind("library-new", Mark.Info, "sem pipelines guardados",
            r => "Ainda não há pipelines guardados; serão feitos agora");
        public static readonly Kind LibrarySaved = new Kind("library-saved", Mark.Info, "pipelines guardados no disco",
            r => "Pipelines guardados no disco: " + Bytes(r.A));
        public static readonly Kind LibraryLost = new Kind("library-lost", Mark.Warn, "pipelines não guardados",
            r => "os pipelines não puderam ser guardados no disco (" + r.Note + ")", "Serão feitos de novo na próxima vez. Veja se há espaço no disco.");
        public static readonly Kind Ready = new Kind("ready", Mark.Good, "pipelines prontos",
            r => "Pipelines em dia: " + Say.Count(r.B) + " vindos do disco, " + Say.Count(r.A) + " feitos agora",
            "Nenhum pipeline esperando neste momento. Os do disco já existiam de outras partidas.");
        public static readonly Kind Early = new Kind("early", Mark.Info, "pipelines conhecidos",
            r => "Pipelines conhecidos de antes: " + Say.Count(r.A));

        /// Shaders that do not translate, by the translator's reason.
        public static Kind Untranslated(string reason)
        {
            return ByReason("untranslated", reason, (id, text) => new Kind(id, Mark.Warn, "shaders não traduzidos (" + text + ")",
                r => Of(r, "1 shader não traduzido (" + text + ")", "shaders não traduzidos (" + text + ")"), WhatShader + Drops));
        }

        // Limits of the card.
        public static readonly Kind Waves = new Kind("waves", Mark.Warn, "ondas de 64",
            r => "a placa só trabalha em ondas de 32; parte dos shaders de 64 fica de fora", Drops);
        public static readonly Kind ViewHeap = new Kind("view-heap", Mark.Warn, "limite de descritores",
            r => Say.Known(r.A) ? "a placa guarda menos descritores do que o jogo pede (faltam " + Say.Count(r.A) + ")"
                : "a placa guarda menos descritores do que os shaders esperam",
            "Descritor é o endereço de uma textura ou de um buffer. Se faltarem, texturas podem sair erradas.");
        public static readonly Kind Slots = new Kind("slots", Mark.Warn, "falta de espaço para descritores",
            r => Of(r, "faltou espaço para descritores; um desenho ficou sem", "vezes faltou espaço para descritores; desenhos ficaram sem"), Drops);
        public static readonly Kind PastHeap = new Kind("past-heap", Mark.Warn, "descritores além do limite",
            r => Of(r, "1 descritor além do limite da placa", "descritores além do limite da placa"), Drops);
        public static readonly Kind Views = new Kind("views", Mark.Warn, "visões recusadas",
            r => Of(r, "1 visão de textura ou buffer que a placa não aceita", "visões de textura ou buffer que a placa não aceita"), Drops);
        public static readonly Kind Stencil = new Kind("stencil", Mark.Warn, "estêncil",
            r => "a placa não grava estêncil pelo shader; cópias de profundidade vão sem ele", "Sombras e recortes podem sair diferentes.");
        public static readonly Kind Limit = new Kind("limit", Mark.Warn, "limites da placa",
            r => "a placa não tem: " + r.Note);

        // Work that does not run.
        public static readonly Kind Dispatch = new Kind("dispatch", Mark.Warn, "trabalhos de computação descartados",
            r => Of(r, "1 trabalho de computação descartado (" + r.Note + ")", "trabalhos de computação descartados (" + r.Note + ")"), Drops);
        public static readonly Kind Indirect = new Kind("indirect", Mark.Warn, "desenhos indiretos descartados",
            r => Of(r, "1 tipo de desenho indireto descartado", "tipos de desenho indireto descartados"),
            "Desenho indireto é o que a própria GPU decide. " + Drops);
        public static readonly Kind NotDone = new Kind("not-done", Mark.Warn, "funções do console ainda não feitas",
            r => Of(r, "função do console ainda não feita: " + r.Note, "funções do console ainda não feitas (a última: " + r.Note + ")"),
            "Quase sempre sem efeito. Se algo não funcionar no jogo, estas são as primeiras suspeitas.");
        public static readonly Kind Unimplemented = new Kind("unimplemented", Mark.Error, "funções que não existem no Vyce",
            r => Of(r, "o jogo chamou uma função que não existe no Vyce: " + r.Note,
                "chamadas a funções que não existem no Vyce (a última: " + r.Note + ")"),
            "O jogo para quando isso acontece.");
        public static readonly Kind Dll = new Kind("dll", Mark.Warn, "bibliotecas recusadas",
            r => Of(r, "biblioteca do Windows recusada ao jogo: " + r.Note, "bibliotecas do Windows recusadas ao jogo (a última: " + r.Note + ")"),
            "É uma proteção do Vyce e costuma ser normal.");
        public static readonly Kind DllLost = new Kind("dll-lost", Mark.Error, "bibliotecas do jogo que não abriram",
            r => "uma biblioteca do jogo não abriu: " + r.Note);

        // Time.
        public static readonly Kind Slow = new Kind("slow", Mark.Warn, "envios lentos à GPU",
            r => Of(r, "a GPU levou " + Say.Ms(r.A) + " em um envio", "envios lentos à GPU (o pior levou " + Say.Ms(r.A) + ")"),
            "A GPU passou de 250 ms em um envio só: o jogo engasga. Perto de 2 s o Windows reinicia o driver de vídeo.", 30);
        public static readonly Kind Waited = new Kind("waited", Mark.Warn, "esperas sem resposta da GPU",
            r => Of(r, "uma espera pela GPU não terminou (" + r.Note + ")", "esperas pela GPU não terminaram (" + r.Note + ")"));

        // Memory.
        public static readonly Kind InPlace = new Kind("in-place", Mark.Info, "memória usada direto pela GPU",
            r => "Memória do jogo usada direto pela GPU: " + Say.Count(r.A) + (r.A == 1 ? " área" : " áreas"), null, 60);
        public static readonly Kind Copied = new Kind("copied", Mark.Info, "memória copiada para a GPU",
            r => "Memória do jogo copiada para a GPU: " + Say.Count(r.A) + (r.A == 1 ? " área" : " áreas"), null, 60);
        public static readonly Kind OnCard = new Kind("on-card", Mark.Info, "buffers na placa",
            r => Of(r, "1 buffer grande passou para a memória da placa", "buffers grandes passaram para a memória da placa"));
        public static readonly Kind NoCard = new Kind("no-card", Mark.Warn, "buffers fora da placa",
            r => "os buffers grandes ficam na RAM: o recurso da placa não abriu");
        public static readonly Kind NoMemory = new Kind("no-memory", Mark.Error, "falta de memória",
            r => Of(r, "faltou memória: " + r.Note, "vezes faltou memória (a última: " + r.Note + ")"),
            "Feche outros programas. Se for a memória da placa, diminua o limite de texturas.");
        public static readonly Kind Commit = new Kind("commit", Mark.Error, "memória do Windows esgotada",
            r => "acabou a memória do Windows (RAM e arquivo de paginação)",
            "Aumente o arquivo de paginação ou libere espaço no disco em que ele fica.");

        // Ends.
        public static readonly Kind GpuGone = new Kind("gpu-gone", Mark.Error, "placa de vídeo parou",
            r => "a placa de vídeo parou de responder" + (r.Note != null ? " (" + r.Note + ")" : ""),
            "O Windows reiniciou o driver de vídeo e o jogo não tem como continuar.");
        public static readonly Kind Crash = new Kind("crash", Mark.Error, "travamentos",
            r => "o jogo travou: " + r.Note, "O registro completo, com a lista de chamadas, está abaixo e no arquivo de log.");
        public static readonly Kind FirstChance = new Kind("first-chance", Mark.Warn, "exceções internas",
            r => Of(r, "exceção interna: " + r.Note, "exceções internas (a última: " + r.Note + ")"),
            "Pode ser tratada pelo próprio jogo. Se ele travar em seguida, foi esta.");
        public static readonly Kind Aborted = new Kind("aborted", Mark.Error, "o jogo abortou",
            r => "o jogo se encerrou por um erro interno dele");
        public static readonly Kind Panic = new Kind("panic", Mark.Error, "falhas internas do Vyce",
            r => "falha interna do Vyce: " + r.Note);
        public static readonly Kind Fatal = new Kind("fatal", Mark.Error, "erros ao iniciar",
            r => r.Note);

        static readonly Dictionary<string, Kind> Reasons = new Dictionary<string, Kind>(StringComparer.Ordinal);

        static Kind ByReason(string id, string reason, Func<string, string, Kind> make)
        {
            id += ":" + reason;
            Kind kind;
            lock (Reasons)
            {
                if (!Reasons.TryGetValue(id, out kind))
                {
                    // Reasons with numbers in them must not pile up.
                    if (Reasons.Count > 300) id = "other";
                    if (!Reasons.TryGetValue(id, out kind)) Reasons[id] = kind = make(id, reason);
                }
            }
            return kind;
        }

        /// A warning or error no rule knows: told in its own words.
        public static Kind Other(Level level, string message)
        {
            Mark mark = level == Level.Error ? Mark.Error : Mark.Warn;
            return ByReason(mark == Mark.Error ? "error" : "warn", Lines.Shape(message), (id, shape) => new Kind(id, mark, shape,
                r => r.Note + (r.Count > 1 ? " (" + N(r) + " vezes)" : ""), "Mensagem do Vyce, em inglês, sem tradução no painel.", 60));
        }

        /// Why a shader does not translate.
        public static string Reason(string error)
        {
            if (error == null) return "motivo não informado";
            if (error.Contains("sharing LDS")) return "grupos de 64 que dividem memória";
            if (error.Contains("wave64") || error.Contains("64-lane")) return "ondas de 64, que a placa não tem";
            if (error.Contains("SGPR")) return "endereço que o tradutor não sabe seguir";
            if (error.Contains("mesh outputs")) return "malha maior do que a placa aceita";
            int at = error.IndexOf(": ", StringComparison.Ordinal);
            string reason = at < 0 ? error : error.Substring(at + 2);
            return Say.Cut(Lines.Shape(reason.Replace(" is not supported", "")), 60);
        }
    }
}
namespace Vyce.Monitor
{
    using System;
    using System.Collections.Generic;
    using System.IO;
    using System.Runtime.InteropServices;
    using System.Text;
    using System.Threading;

    /// What the logs said about how the run ended.
    public sealed class Hints
    {
        public bool WindowClosed, NoMemory, Panic, Aborted;
        public string GpuGone, Crash, Exit;
    }

    /// All that is known of the run. The reading thread adds to it; whoever
    /// reads it holds `Sync`.
    public sealed class Session
    {
        const int MostSamples = 2400;
        public readonly object Sync = new object();
        /// Grows with every change a window would show.
        public int Version;
        public readonly Ring Log = new Ring();
        public readonly Journal Journal = new Journal();
        public readonly Hints Hints = new Hints();
        public readonly List<Sample> Samples = new List<Sample>();
        public LoadReport Load;
        public string Stage = "";
        public DateTime StageAt;
        /// How long the last load took.
        public double Took = double.NaN;
        /// Pipelines done when the world began to wait for some.
        public double BacklogFrom = double.NaN;
        public string Build, BuildHash, Console, Adapter, Driver, Path, Output, GpuTop;
        public double AdapterMb = double.NaN, Workers = double.NaN, Commit = double.NaN;
        public double TextureCap = double.NaN;
        public int Shaders, Pipelines, Refused, Files;
        public int BadTelemetry, Telemetry, LoadLines, LinesRead;
        /// When the process started, if known.
        public DateTime Started = DateTime.MinValue;
        public DateTime FirstAt = DateTime.MinValue, LastLineAt = DateTime.MinValue;
        /// The process the title went on in.
        public int Relaunched;
        readonly Guess guess = new Guess();
        bool guessing = true;
        DateTime clock = DateTime.MinValue;
        DateTime origin = DateTime.MinValue;
        Row follow;

        public Sample Last
        {
            get { return Samples.Count == 0 ? null : Samples[Samples.Count - 1]; }
        }

        /// Adds a line. `live` is false for what was in the file before the
        /// panel opened it; `now` is the time it was read, and `last` the
        /// latest it can have been written.
        public void Add(string raw, bool error, bool live, DateTime now, DateTime last)
        {
            if (raw.Length == 0) return;
            LinesRead++;
            Version++;
            LastLineAt = live ? now : last;
            if (raw.StartsWith("VYCE_", StringComparison.Ordinal))
            {
                Report(raw, live, now, last);
                return;
            }
            // A report in the middle of another thread's line.
            int at = raw.IndexOf("VYCE_TELEMETRY {", StringComparison.Ordinal);
            if (at < 0) at = raw.IndexOf("VYCE_LOADING {", StringComparison.Ordinal);
            if (at > 0)
            {
                Report(raw.Substring(at), live, now, last);
                raw = raw.Substring(0, at);
            }
            string text = Lines.Clean(raw);
            if (text.Trim().Length == 0) return;
            DateTime stamp;
            int from = Lines.Stamp(text, out stamp);
            string target, body;
            Level level = Lines.Parts(text, from, out target, out body);
            if (from > 0)
            {
                Stamped(stamp);
                follow = null;
            }
            else stamp = live ? now : clock > DateTime.MinValue ? clock : last;
            if (FirstAt == DateTime.MinValue) FirstAt = stamp;
            LogLine line = new LogLine { At = stamp, Level = level, Text = from > 0 ? text.Substring(from).TrimStart() : text };
            Log.Add(line);
            if (target != null) line.Noise = Runtime(line, target, body);
            else line.Noise = Plain(line, error, text);
        }

        /// The time of a stamped line.
        void Stamped(DateTime stamp)
        {
            // A time reckoned from frames cannot be after the next line's.
            Sample sample = Last;
            if (sample != null && stamp < sample.At && (sample.At - stamp).TotalSeconds < 5 && reckoned) sample.At = stamp;
            reckoned = false;
            if (stamp > clock) clock = stamp;
        }

        bool reckoned;

        /// When a report was written.
        DateTime Written(Dictionary<string, object> fields, double frames, double ms, bool live, DateTime now, DateTime last)
        {
            DateTime at = now;
            if (!live)
            {
                at = clock > DateTime.MinValue ? clock : last;
                Sample before = Last;
                if (before != null && Say.Known(ms) && frames * ms < 600000)
                {
                    // As long after the one before as its frames took.
                    DateTime reckon = before.At.AddMilliseconds(frames * ms);
                    reckoned = reckon > at;
                    if (reckoned) at = reckon;
                }
                object t;
                if (fields.TryGetValue("t_s", out t) && t is double)
                {
                    // Seconds since the process started; another's if they
                    // do not agree with the log's own times.
                    if (origin == DateTime.MinValue)
                    {
                        origin = at.AddSeconds(-(double)t);
                        if (Started > DateTime.MinValue && Math.Abs((Started - origin).TotalSeconds) < 60) origin = Started;
                    }
                    at = origin.AddSeconds((double)t);
                    reckoned = false;
                }
            }
            if (at > now) at = now;
            if (at > clock) clock = at;
            return at;
        }

        void Report(string raw, bool live, DateTime now, DateTime last)
        {
            string text = Lines.Clean(raw);
            int space = text.IndexOf(' ');
            string name = space < 0 ? text : text.Substring(0, space);
            string rest = space < 0 ? "" : text.Substring(space + 1);
            DateTime at = live ? now : clock > DateTime.MinValue ? clock : last;
            if (name == "VYCE_TELEMETRY") at = OnTelemetry(rest, live, now, last);
            else if (name == "VYCE_LOADING") at = OnLoading(rest, live, now, last);
            else if (name == "VYCE_GPU") OnGpu(rest, at, text);
            else if (name == "VYCE_PRESENT") OnPresent(rest, at, text);
            if (FirstAt == DateTime.MinValue) FirstAt = at;
            Log.Add(new LogLine { At = at, Level = Level.Debug, Noise = name != "VYCE_PRESENT", Text = text });
        }

        DateTime OnTelemetry(string rest, bool live, DateTime now, DateTime last)
        {
            Dictionary<string, object> fields = Json.Read(rest, 0);
            object what = null;
            if (fields == null || fields.TryGetValue("event", out what) && !"frame".Equals(what))
            {
                if (fields == null) BadTelemetry++;
                return live ? now : clock;
            }
            Sample s = new Sample(fields);
            if (!Say.Known(s.Fps) || !Say.Known(s.Ms))
            {
                BadTelemetry++;
                return live ? now : clock;
            }
            Sample before = Last;
            double frames = before == null ? s.Total : s.Total - before.Total;
            s.Frames = frames >= 1 && frames < 100000 ? frames : 1;
            s.At = Written(fields, before == null ? 0 : s.Frames, s.Ms, live, now, last);
            if (before != null && s.At < before.At) s.At = before.At;
            Telemetry++;
            Samples.Add(s);
            if (Samples.Count > MostSamples) Samples.RemoveRange(0, MostSamples / 4);
            if (Say.Known(s["tex_cap"])) TextureCap = s["tex_cap"];
            string said = "";
            double w = s["out_w"], h = s["out_h"], sw = s["scene_w"], sh = s["scene_h"], planes = s["planes"];
            if (w > 0 && h > 0) said = "saída " + Say.Number(w, "0") + "x" + Say.Number(h, "0");
            if (sw > 0 && sh > 0) said += (said == "" ? "" : ", ") + "cena " + Say.Number(sw, "0") + "x" + Say.Number(sh, "0");
            if (planes > 1) said += (said == "" ? "" : ", ") + Say.Number(planes, "0") + " camadas";
            if (said != "") Output = said;
            object console;
            if (Console == null && fields.TryGetValue("console", out console) && console is string)
                Console = ((string)console).Replace("series-", "Xbox Series ").Replace("Series s", "Series S").Replace("Series x", "Series X");
            DateTime since;
            if (guessing && guess.Add(s, out since)) SetStage(guess.Stage, since);
            return s.At;
        }

        DateTime OnLoading(string rest, bool live, DateTime now, DateTime last)
        {
            Dictionary<string, object> fields = Json.Read(rest, 0);
            if (fields == null)
            {
                BadTelemetry++;
                return live ? now : clock;
            }
            LoadReport r = new LoadReport(fields, Written(fields, 0, double.NaN, live, now, last));
            LoadLines++;
            guessing = false;
            DateTime since = r.At;
            double elapsed = r["elapsed_s"];
            if (Stage != r.Stage && Say.Known(elapsed) && elapsed > 0 && elapsed < 36000) since = since.AddSeconds(-elapsed);
            if (r.Stage != "") SetStage(r.Stage, since);
            double waiting = r["pipelines_waiting"], done = r["pipelines_done"];
            if (r.Stage != "world" || waiting < 4) BacklogFrom = double.NaN;
            else if (waiting >= 8 && !Say.Known(BacklogFrom)) BacklogFrom = done;
            Load = r;
            return r.At;
        }

        /// The passes the GPU spends most on, in whatever form a build tells them.
        void OnGpu(string rest, DateTime at, string line)
        {
            string top = rest.StartsWith("{", StringComparison.Ordinal) ? Passes(Json.Read(rest, 0)) : Say.Cut(rest, 160);
            if (string.IsNullOrEmpty(top)) return;
            GpuTop = top;
            Journal.Put(Kinds.GpuTop, at, line).Note = top;
        }

        static string Passes(Dictionary<string, object> fields)
        {
            if (fields == null) return null;
            List<KeyValuePair<double, string>> passes = new List<KeyValuePair<double, string>>();
            foreach (object value in fields.Values)
            {
                List<object> items = value as List<object>;
                Dictionary<string, object> map = value as Dictionary<string, object>;
                if (items != null)
                {
                    foreach (object item in items)
                    {
                        Dictionary<string, object> pass = item as Dictionary<string, object>;
                        if (pass != null) AddPass(passes, Word(pass), Milliseconds(pass));
                    }
                }
                else if (map != null)
                {
                    foreach (KeyValuePair<string, object> m in map)
                        if (m.Value is double) AddPass(passes, m.Key, (double)m.Value);
                }
            }
            passes.Sort((a, b) => b.Key.CompareTo(a.Key));
            StringBuilder text = new StringBuilder();
            for (int k = 0; k < passes.Count && k < 4; k++)
                text.Append(k == 0 ? "" : " · ").Append(passes[k].Value).Append(' ').Append(Say.Ms(passes[k].Key));
            return text.ToString();
        }

        static void AddPass(List<KeyValuePair<double, string>> passes, string name, double ms)
        {
            if (name != null && Say.Known(ms)) passes.Add(new KeyValuePair<double, string>(ms, Say.Cut(name, 48)));
        }

        static string Word(Dictionary<string, object> pass)
        {
            foreach (string key in new[] { "name", "pass", "what", "label", "pipeline", "shader", "key", "kind" })
            {
                object v;
                if (pass.TryGetValue(key, out v) && v is string) return (string)v;
            }
            return null;
        }

        static double Milliseconds(Dictionary<string, object> pass)
        {
            foreach (string key in new[] { "ms", "gpu_ms", "ms_per_frame", "time_ms", "us" })
            {
                object v;
                if (pass.TryGetValue(key, out v) && v is double) return key == "us" ? (double)v / 1000 : (double)v;
            }
            return double.NaN;
        }

        void OnPresent(string rest, DateTime at, string line)
        {
            string path = Lines.Field(rest, "path") ?? "";
            Path = path == "copy" ? "cópia" : path == "compose" ? "composição de camadas" : path;
            Journal.Put(Kinds.PicturePath, at, line).Note = Path;
        }

        void SetStage(string stage, DateTime at)
        {
            if (stage == Stage) return;
            string before = Stage;
            DateTime beforeAt = StageAt;
            Stage = stage;
            StageAt = at;
            Kind kind = stage == "start" ? Kinds.Start : stage == "idle" ? Kinds.Idle : stage == "load" ? Kinds.Load
                : stage == "again" ? Kinds.Again : stage == "world" ? Kinds.World : null;
            if (kind == null) return;
            Journal.Close();
            Row row = Journal.Put(kind, at, null);
            if (stage == "world" && (before == "load" || before == "again"))
            {
                Took = (at - beforeAt).TotalSeconds;
                row.A = Took;
            }
        }

        /// The panel's own events.
        public Row Tell(Kind kind, DateTime at, string note)
        {
            Version++;
            Row row = Journal.Put(kind, at, null);
            row.Note = note;
            return row;
        }

        // ---- lines of the runtime's log ----

        /// Pipelines refused with none made around them yet.
        int refusedBefore;
        DateTime refusedAt = DateTime.MinValue;
        string said;
        LogLine now;

        bool Is(string start)
        {
            return said.StartsWith(start, StringComparison.Ordinal);
        }

        Row Put(Kind kind)
        {
            return Journal.Put(kind, now.At, now.Text);
        }

        Row Put(Kind kind, string note)
        {
            Row row = Journal.Put(kind, now.At, now.Text);
            row.Note = note;
            return row;
        }

        string F(string name)
        {
            return Lines.Field(said, name);
        }

        double V(string name)
        {
            return Lines.Value(said, name);
        }

        string Size2(string width, string height)
        {
            return Say.Number(V(width), "0") + "x" + Say.Number(V(height), "0");
        }

        /// Makes an event of a line if it is one. True for lines that only
        /// matter when everything is shown.
        bool Runtime(LogLine line, string target, string body)
        {
            now = line;
            said = body;
            if (target == "title")
            {
                Put(Kinds.Said, Say.Cut(body, 120));
                return false;
            }
            if (line.Level <= Level.Debug && line.Level != Level.Plain) return true;
            if (line.Level == Level.Info) return Info(target);
            return Trouble(line.Level);
        }

        bool Info(string target)
        {
            if (Is("shader translated"))
            {
                Shaders++;
                Journal.Put(Kinds.Shader, now.At, null);
                return true;
            }
            if (Is("first bind")) return true;
            if (Is("host pipeline name=") || Is("host mesh pipeline name=") || Is("host pipeline shaders="))
            {
                Pipelines++;
                Row row = Journal.Put(Kinds.Pipeline, now.At, null);
                if (row.Count == 1 && refusedBefore > 0 && (now.At - refusedAt).TotalSeconds <= Kinds.Pipeline.Gap) row.B = refusedBefore;
                refusedBefore = 0;
                return true;
            }
            if (Is("commit "))
            {
                Commit = Lines.After(said, "commit ");
                return true;
            }
            if (Is("dstorage: file opened"))
            {
                Files++;
                Put(Kinds.Files, Lines.FileName(F("path") ?? "?"));
                return false;
            }
            if (Is("dstorage: workers")) Workers = V("workers");
            if (Is("title memory in place") && !Is("title memory in place was"))
            {
                Put(Kinds.InPlace).Most(V("count"));
                return true;
            }
            if (Is("title memory not usable in place"))
            {
                Put(Kinds.Copied).Most(V("count"));
                return true;
            }
            if (Is("host pipelines all made"))
            {
                Row row = Put(Kinds.Ready);
                row.A = V("made");
                row.B = V("loaded");
                return false;
            }
            if (Is("host pipeline library loaded")) { Put(Kinds.LibraryLoaded).A = V("bytes"); return false; }
            if (Is("host pipeline library saved")) { Put(Kinds.LibrarySaved).A = V("bytes"); return false; }
            if (Is("host pipeline library rejected")) { Put(Kinds.LibraryRejected); return false; }
            if (Is("no host pipeline library yet")) { Put(Kinds.LibraryNew); return false; }
            if (Is("title pipelines to start early")) { Put(Kinds.Early).A = V("pipelines"); return false; }
            if (Is("host adapter"))
            {
                Adapter = F("name");
                AdapterMb = V("vram_mb");
                SetDriver(F("driver"));
                Put(Kinds.Adapter, Adapter + " (" + Say.Size(AdapterMb) + ")");
                return false;
            }
            if (Is("host device on the software adapter")) { Put(Kinds.Software); return false; }
            if (Is("host D3D12 device ready"))
            {
                SetDriver(F("driver"));
                Put(Kinds.Device);
                return false;
            }
            if (Is("host driver") || Is("driver")) SetDriver(F("driver") ?? F("version"));
            if (Is("D3D12 GPU upload heap")) { Put(Kinds.Bar, F("gpu_upload") == "true" ? "ligado" : "desligado"); return false; }
            if (Is("title window fitted to the screen"))
            {
                string to = F("to") ?? "";
                Put(Kinds.Window, Size2("width", "height") + ", mostrada em " + to.Trim('(', ')').Replace(", ", "x"));
                return false;
            }
            if (Is("presenting to a window")) { Put(Kinds.Picture, Size2("width", "height")); return false; }
            if (Is("presentation formats")) { Put(Kinds.PicturePath, F("path") == "copy" ? "cópia" : "composição de camadas"); return true; }
            if (Is("several planes presented")) { Put(Kinds.PicturePath, "camadas (" + F("planes") + ")"); return false; }
            if (Is("frame presented"))
            {
                if (V("frame") == 1) Put(Kinds.FirstFrame);
                return true;
            }
            if (Is("video memory "))
            {
                // "video memory now: 6100 of 7114 MiB (86 %, 91 % for streamed textures, which are cut to 512 ...".
                double used = Lines.After(said, ": "), budget = Lines.After(said, " of "), cap = Lines.After(said, "cut to ");
                if (Say.Known(cap)) TextureCap = cap;
                string note = Say.Size(used) + " de " + Say.Size(budget) + (budget > 0 ? " (" + Say.Share(used / budget) + ")" : "");
                if (Say.Known(cap)) note += cap > 0 ? ", texturas novas até " + Say.Number(cap, "0") : ", texturas sem limite";
                Put(Kinds.VideoMemory, note);
                return false;
            }
            if (Is("a load is done"))
            {
                string kind = F("kind");
                Row row = Put(Kinds.LoadDone, kind == "start" ? "Início do jogo terminado" : kind == "again" ? "Lugar carregado" : "Mundo carregado");
                row.A = V("seconds");
                row.B = V("mib");
                return false;
            }
            if (Is("a load begins")) return false;
            if (Is("input device ") && !Is("input device callback"))
            {
                double device = V("device");
                string what = device == 4 ? "Teclado" : device == 5 ? "Mouse" : "Controle";
                what += F("connected") == "true" ? " conectado" : " desconectado";
                Row row = Journal.Open(Kinds.Pad, now.At);
                Put(Kinds.Pad, row.Note == null || row.Note == what ? what : row.Note + ", " + what.ToLowerInvariant());
                return false;
            }
            if (Is("DualSense connected")) { Put(Kinds.Pad, "DualSense conectado"); return false; }
            if (Is("audio decoder connected") || Is("audio DSP connected")) { Put(Kinds.Sound); return false; }
            if (target.EndsWith("::net", StringComparison.Ordinal) && F("destination") != null) { Put(Kinds.Net); return false; }
            if (Is("indirect execution dropped")) { Put(Kinds.Indirect); return false; }
            if (Is("large buffer placed in VRAM") || Is("loose memory placed in VRAM")) { Put(Kinds.OnCard); return true; }
            if (Is("window closed"))
            {
                Hints.WindowClosed = true;
                Put(Kinds.Closing);
                return false;
            }
            if (Is("title suspended; ending")) { Put(Kinds.Ending, "Jogo suspenso; fechando"); return false; }
            if (Is("title did not finish suspending")) { Put(Kinds.Ending, "O jogo demorou a suspender e foi fechado assim mesmo"); return false; }
            if (Is("relaunched"))
            {
                double pid = V("pid");
                if (pid > 0) Relaunched = (int)pid;
                Put(Kinds.Relaunch, "PID " + Say.Number(pid, "0"));
                return false;
            }
            return Quiet(target);
        }

        /// Lines that repeat or say nothing to whoever only looks on.
        bool Quiet(string target)
        {
            if (target.StartsWith("victory_loader", StringComparison.Ordinal) || target.EndsWith("::memory", StringComparison.Ordinal)
                || target.EndsWith("::shadow", StringComparison.Ordinal) || target.EndsWith("::timeline", StringComparison.Ordinal)
                || target.EndsWith("::twin", StringComparison.Ordinal) || target.EndsWith("::frame", StringComparison.Ordinal)
                || target.EndsWith("::display", StringComparison.Ordinal) || target.EndsWith("::xgameruntime", StringComparison.Ordinal))
                return true;
            return Is("[") || Is("dstorage: ") || Is("bindless plan per frame") || Is("shaders from the cache")
                || Is("host descriptor heap") || Is("input device callback") || Is("first coprocessor command")
                || Is("tiles given back") || Is("copies per frame") || Is(" ") || Is("GPU time") || Is("GPU wait")
                || Is("sparse writes since") || Is("dropped ");
        }

        void SetDriver(string driver)
        {
            if (string.IsNullOrEmpty(driver) || driver == "unknown") return;
            Driver = DriverName(driver);
        }

        /// "10de 32.0.16.1088" is what NVIDIA calls 610.88.
        public static string DriverName(string driver)
        {
            string version = driver;
            int space = driver.LastIndexOf(' ');
            bool nvidia = driver.StartsWith("10de", StringComparison.OrdinalIgnoreCase);
            if (space >= 0) version = driver.Substring(space + 1);
            string[] parts = version.Split('.');
            int third, fourth;
            if (nvidia && parts.Length == 4 && int.TryParse(parts[2], out third) && int.TryParse(parts[3], out fourth))
            {
                int number = third % 10 * 10000 + fourth;
                return number / 100 + "." + (number % 100).ToString("00");
            }
            return version;
        }

        bool Trouble(Level level)
        {
            if (Is("shader does not translate")) { Put(Kinds.Untranslated(Kinds.Reason(F("error")))); return false; }
            if (Is("host pipeline refused") || Is("host mesh pipeline refused") || Is("host root signature refused"))
            {
                Refused++;
                string error = F("error") ?? "sem motivo";
                int at = error.IndexOf(" (0x", StringComparison.Ordinal);
                Put(Kinds.Refused, (at > 0 ? error.Substring(0, at) : error).TrimEnd('.').ToLowerInvariant());
                // Told where the pipelines made are told, too.
                Row made = Journal.Peek(Kinds.Pipeline, now.At);
                if (made != null) made.B = Say.Known(made.B) ? made.B + 1 : 1;
                else
                {
                    refusedBefore = (now.At - refusedAt).TotalSeconds > Kinds.Pipeline.Gap ? 1 : refusedBefore + 1;
                    refusedAt = now.At;
                }
                return true;
            }
            if (Is("no host pipeline: ")) { Put(Kinds.NoPipeline, Say.Cut(Lines.Message(said).Substring(18), 60)); return false; }
            if (Is("a compute pipeline without DXIL")) { Put(Kinds.NoPipeline, "sem código"); return false; }
            if (Is("the driver's compiler crashed")) { Put(Kinds.Crashed); return false; }
            if (Is("the driver's compiler does not end")) { Put(Kinds.Endless); return false; }
            if (Is("pipelines this driver's compiler crashed on"))
            {
                SetDriver(F("driver"));
                Put(Kinds.LeftOut).A = V("pipelines");
                return false;
            }
            if (Is("host pipeline library not")) { Put(Kinds.LibraryLost, Short(F("error"))); return false; }
            if (Is("no host pipeline library")) { Put(Kinds.LibraryLost, "recurso ausente"); return false; }
            if (Is("host wave sizes exclude") || Is("the host has no 64-lane waves")) { Put(Kinds.Waves); return false; }
            if (Is("host view heap capped")) { Put(Kinds.ViewHeap).Most(V("lost")); return false; }
            if (Is("a view heap shorter")) { Put(Kinds.ViewHeap); return false; }
            if (Is("no free dynamic descriptor slots") || Is("a table larger than the dynamic slots")
                || Is("descriptor table did not fit") || Is("a constant table did not fit") || Is("no host descriptor heap"))
            {
                Put(Kinds.Slots);
                return false;
            }
            if (Is("title slot past the host heap")) { Put(Kinds.PastHeap); return false; }
            if (Is("view the host does not allow") || Is("a buffer view the host does not allow") || Is("a view off its elements")
                || Is("a bindless unordered access view too big"))
            {
                Put(Kinds.Views);
                return false;
            }
            if (Is("the host takes no stencil")) { Put(Kinds.Stencil); return false; }
            if (Is("the host GPU has no depth bounds")) { Put(Kinds.Limit, "teste de limites de profundidade"); return false; }
            if (Is("the host cannot read 32-bit textures")) { Put(Kinds.Limit, "leitura de cores em formato compartilhado"); return false; }
            if (Is("slow host submission")) { Put(Kinds.Slow).Most(V("ms")); return false; }
            if (Is("a dispatch without a host pipeline")) { Put(Kinds.Dispatch, "sem pipeline"); return false; }
            if (Is("a dispatch writing through a null")) { Put(Kinds.Dispatch, "sem destino"); return false; }
            if (Is("method not implemented")) { Put(Kinds.NotDone, F("object") + " " + F("slot")); return false; }
            if (Is("runtime method not implemented")) { Put(Kinds.NotDone, "runtime " + F("slot")); return false; }
            if (Is("runtime API area not implemented")) { Put(Kinds.NotDone, "área do runtime"); return false; }
            if (Is("unknown interface")) { Put(Kinds.NotDone, "interface de " + F("object")); return false; }
            if (Is("device feature query not implemented")) { Put(Kinds.NotDone, "consulta " + F("feature")); return false; }
            if (Is("GetProcAddress")) { Put(Kinds.NotDone, (F("module") ?? "") + " " + (F("name") ?? F("ordinal"))); return false; }
            if (Is("unimplemented import called")) { Put(Kinds.Unimplemented, F("dll") + " " + F("symbol")); return false; }
            if (Is("a host DLL outside the host policy")) { Put(Kinds.Dll, F("dll")); return false; }
            if (Is("mapping a title DLL failed")) { Put(Kinds.DllLost, F("dll")); return false; }
            if (Is("dstorage: opening a file failed")) { Put(Kinds.FileFailed, Lines.FileName(F("path") ?? "?")); return false; }
            if (Is("dstorage: request failed"))
            {
                string why = Lines.Message(said);
                Put(Kinds.ReadFailed, Say.Cut(why.Length > 26 ? why.Substring(26) : why, 60));
                return false;
            }
            if (Is("dstorage: BCPack requests fail")) { Put(Kinds.ReadFailed, "texturas compactadas do console"); return false; }
            if (Is("host submission not waited for")) { Put(Kinds.Waited, "envio"); return false; }
            if (Is("host fence wait failed")) { Put(Kinds.Waited, "cerca"); return false; }
            if (Is("host async")) { Put(Kinds.Waited, "envio adiantado"); return false; }
            if (Is("a wait was never met")) { Put(Kinds.Waited, "fila parada"); return false; }
            if (Is("VRAM card buffer engine unavailable")) { Put(Kinds.NoCard); return false; }
            if (Is("no upload buffer") || Is("no readback buffer") || Is("no shared buffer") || Is("no unordered-access buffer")
                || Is("no host texture") || Is("no memory for copies") || Is("no sparse buffer")
                || Is("XMemAllocatePhysicalPages failed") || Is("committing physical pages failed"))
            {
                Put(Kinds.NoMemory, Memory());
                return false;
            }
            if (Is("cannot present this format")) { Put(Kinds.NoPicture, "formato " + F("format")); return false; }
            if (Is("no swap chain") || Is("swap chain not resized")) { Put(Kinds.NoPicture, Short(F("error"))); return false; }
            if (Is("presented planes not put together")) { Put(Kinds.NoPicture, "camadas"); return false; }
            if (Is("no host D3D12 device")) { Put(Kinds.NoDevice); return false; }
            if (Is("command lists submitted; no host GPU")) { Put(Kinds.NoDevice); return false; }
            if (Is("host GPU device removed") || Is("host GPU device gone"))
            {
                Hints.GpuGone = Removal(said);
                Put(Kinds.GpuGone, Hints.GpuGone);
                return false;
            }
            if (Is("title exits") || Is("title terminates the process"))
            {
                Hints.Exit = F("code");
                Put(Kinds.Exits, Hints.Exit);
                return false;
            }
            if (Is("title aborts") || Is("title calls terminate") || Is("title passed an invalid parameter"))
            {
                Hints.Aborted = true;
                Put(Kinds.Aborted);
                return false;
            }
            if (Is("no loading progress") || Is("this load was not kept") || Is("the loads before were not read") || Is("loads not kept"))
            {
                Put(Kinds.LoadLost, Short(F("error")));
                return false;
            }
            if (Is("VICTORY_") || Is("VYCE_")) { Put(Kinds.Tuned, Say.Cut(said, 90)); return false; }
            string message = Lines.Message(said);
            if (message.Length == 0) message = said;
            Put(Kinds.Other(level, message), Say.Cut(message, 140));
            return false;
        }

        string Memory()
        {
            string what = Lines.Message(said);
            double size = V("size");
            if (!Say.Known(size)) size = V("len");
            string of = Is("no host texture") ? "textura na placa" : Is("no memory for copies") || Is("no sparse buffer") ? "cópias da memória do jogo"
                : Is("no ") ? "buffer" : "memória do jogo";
            return Say.Known(size) ? of + " de " + Say.Size(size / 1048576) : what.Length == 0 ? "?" : of;
        }

        static string Short(string error)
        {
            return error == null ? "sem motivo" : Say.Cut(error, 60);
        }

        /// The driver's reason in a removal's text: `0x887A0006` and its words.
        static string Removal(string text)
        {
            for (int at = text.IndexOf("0x887", StringComparison.OrdinalIgnoreCase); at >= 0;)
            {
                string hex = text.Substring(at + 2, Math.Min(8, text.Length - at - 2));
                uint code;
                if (uint.TryParse(hex, System.Globalization.NumberStyles.HexNumber, null, out code)) return "0x" + hex.ToUpperInvariant();
                break;
            }
            return null;
        }

        // ---- lines of no log ----

        bool Plain(LogLine line, bool error, string text)
        {
            now = line;
            said = text;
            if (error)
            {
                if (Is("VIctory: ") || Is("thread '") || Is("Error: ") || Is("error: ")) return Head(text);
                if (follow != null)
                {
                    follow.Follow(text);
                    return false;
                }
                if (line.Level >= Level.Warn) Put(Kinds.Other(line.Level, text), Say.Cut(text, 140));
                return false;
            }
            return Standard(text);
        }

        /// The head of a report of several lines.
        bool Head(string text)
        {
            Row row = null;
            if (Is("VIctory: unhandled exception"))
            {
                Hints.Crash = Fault(text);
                row = Put(Kinds.Crash, Hints.Crash);
            }
            else if (Is("VIctory: first-chance exception")) row = Put(Kinds.FirstChance, Fault(text));
            else if (Is("VIctory: host GPU failure"))
            {
                if (Hints.GpuGone == null) Hints.GpuGone = Removal(text) ?? "";
                row = Put(Kinds.GpuGone, Hints.GpuGone.Length == 0 ? null : Hints.GpuGone);
            }
            else if (Is("VIctory: Windows commit limit"))
            {
                Hints.NoMemory = true;
                row = Put(Kinds.Commit);
            }
            else if (Is("VIctory: panic") || Is("thread '"))
            {
                // Rust prints the message on the next line.
                if (Is("thread '") && text.IndexOf("panicked", StringComparison.Ordinal) < 0) return false;
                Hints.Panic = true;
                row = Put(Kinds.Panic, Say.Cut(text.Replace("VIctory: ", ""), 120));
            }
            else if (Is("VIctory: unimplemented import")) row = Put(Kinds.Unimplemented, Say.Cut(text.Substring(30).Replace(" called", ""), 80));
            else if (Is("Error: ") || Is("error: ")) row = Put(Kinds.Fatal, Say.Cut(text.Substring(7), 140));
            follow = row;
            return false;
        }

        /// "acesso inválido à memória em ntdll.dll+0x12d844".
        static string Fault(string text)
        {
            int at = text.IndexOf("exception 0x", StringComparison.Ordinal);
            if (at < 0) return Say.Cut(text, 120);
            string rest = text.Substring(at + 12);
            uint code = 0;
            if (rest.Length >= 8) uint.TryParse(rest.Substring(0, 8), System.Globalization.NumberStyles.HexNumber, null, out code);
            int place = rest.IndexOf(" at ", StringComparison.Ordinal);
            string where = "";
            if (place >= 0)
            {
                where = rest.Substring(place + 4);
                int end = where.IndexOf(' ');
                if (end > 0) where = where.Substring(0, end);
            }
            return (Codes.Name(code) ?? "exceção " + Codes.Hex(code)) + (where == "" ? "" : " em " + where);
        }

        /// The runtime's words on standard output, before the title starts.
        bool Standard(string text)
        {
            if (Is("build "))
            {
                string build = text.Substring(5).Trim();
                int at = build.IndexOf(" (", StringComparison.Ordinal);
                Build = at < 0 ? build : build.Substring(0, at);
                BuildHash = at < 0 ? null : build.Substring(at + 2).TrimEnd(')');
                if (Build == "unknown") Put(Kinds.BuildUnknown);
                else Put(Kinds.Build, Build);
            }
            else if (Is("console "))
            {
                Console = text.Substring(7).Trim();
                Put(Kinds.Console, Console);
            }
            else if (Is("mapped at")) Put(Kinds.Mapped);
            else if (Is("title dll")) Put(Kinds.Library, text.Substring(9).Trim().Split(' ')[0]);
            else if (Is("bound "))
            {
                if (text.IndexOf(" 0 missing", StringComparison.Ordinal) >= 0 && text.IndexOf(".exe:", StringComparison.OrdinalIgnoreCase) >= 0)
                    Put(Kinds.Bound);
            }
            else if (Is("  missing ")) Put(Kinds.Missing, text.Substring(10).Trim());
            else if (Is("title exited with"))
            {
                Hints.Exit = text.Substring(17).Trim();
                Put(Kinds.Exits, Hints.Exit);
            }
            return false;
        }
    }

    /// Reads a growing log from its start.
    public sealed class Tail : IDisposable
    {
        const int Chunk = 1 << 20;
        readonly string path;
        readonly bool error;
        readonly byte[] buffer = new byte[Chunk];
        readonly MemoryStream part = new MemoryStream();
        FileStream file;
        long position;
        /// What was in the file when it was first seen; -1 before that.
        long before = -1;
        long lineAt;
        public long Length;

        public Tail(string path, bool error)
        {
            this.path = path;
            this.error = error;
        }

        public long Position { get { return position; } }

        /// Reads what there is, a chunk at most. True if there is more.
        public bool Step(Session session)
        {
            if (file == null)
            {
                if (!File.Exists(path)) return false;
                try
                {
                    file = new FileStream(path, FileMode.Open, FileAccess.Read, FileShare.ReadWrite | FileShare.Delete, 1);
                }
                catch (IOException)
                {
                    // It is being made.
                    return false;
                }
                catch (UnauthorizedAccessException)
                {
                    return false;
                }
            }
            try
            {
                Length = file.Length;
                if (before < 0) before = Length;
                if (Length < position)
                {
                    // Written anew.
                    position = lineAt = before = 0;
                    part.SetLength(0);
                }
                int want = (int)Math.Min(Chunk, Length - position);
                if (want <= 0) return false;
                file.Position = position;
                int read = file.Read(buffer, 0, want);
                if (read <= 0) return false;
                DateTime last = DateTime.UtcNow;
                if (position < before)
                {
                    DateTime written = File.GetLastWriteTimeUtc(path);
                    if (written < last) last = written;
                }
                Split(session, read, last);
                position += read;
                return position < Length;
            }
            catch (IOException)
            {
                Close();
                return false;
            }
        }

        void Split(Session session, int read, DateTime last)
        {
            List<KeyValuePair<long, string>> lines = new List<KeyValuePair<long, string>>();
            int from = 0;
            for (int k = Array.IndexOf(buffer, (byte)10, 0, read); k >= 0; k = from < read ? Array.IndexOf(buffer, (byte)10, from, read - from) : -1)
            {
                string line;
                if (part.Length > 0)
                {
                    part.Write(buffer, from, k - from);
                    line = Encoding.UTF8.GetString(part.GetBuffer(), 0, (int)part.Length);
                    part.SetLength(0);
                }
                else line = Encoding.UTF8.GetString(buffer, from, k - from);
                lines.Add(new KeyValuePair<long, string>(lineAt, line.TrimEnd('\r')));
                from = k + 1;
                lineAt = position + from;
            }
            if (from < read)
            {
                // No line is this long: what it is cannot be told.
                if (part.Length > 8 * Chunk) part.SetLength(0);
                part.Write(buffer, from, read - from);
            }
            DateTime now = DateTime.UtcNow;
            for (int k = 0; k < lines.Count;)
            {
                // In parts, for the window's thread to come in between.
                lock (session.Sync)
                {
                    for (int end = Math.Min(lines.Count, k + 200); k < end; k++)
                    {
                        try
                        {
                            session.Add(lines[k].Value, error, lines[k].Key >= before, now, last);
                        }
                        catch (Exception e)
                        {
                            Faults.Note("linha do log", e);
                        }
                    }
                }
            }
        }

        void Close()
        {
            if (file != null) file.Dispose();
            file = null;
        }

        public void Dispose()
        {
            Close();
            part.Dispose();
        }
    }

    /// The panel's own failures: told, never fatal.
    public static class Faults
    {
        static readonly Dictionary<string, int> Seen = new Dictionary<string, int>(StringComparer.Ordinal);
        public static int Count;
        public static string Last;
        public static DateTime LastAt = DateTime.MinValue;

        public static void Note(string where, Exception e)
        {
            lock (Seen)
            {
                Count++;
                Last = where + ": " + e.Message;
                LastAt = DateTime.UtcNow;
                int n;
                Seen.TryGetValue(where, out n);
                Seen[where] = n + 1;
                if (n >= 5) return;
            }
            try
            {
                // The launcher keeps this beside the logs (.monitor.log).
                Console.Error.WriteLine(DateTime.Now.ToString("HH:mm:ss") + " painel, " + where + ": " + e);
            }
            catch (IOException)
            {
            }
        }
    }

    /// Reads both logs on a thread of its own, so neither a slow disk nor a
    /// burst of lines holds the window up.
    public sealed class Reader : IDisposable
    {
        readonly Session session;
        readonly Tail[] tails;
        readonly AutoResetEvent wake = new AutoResetEvent(false);
        readonly Thread thread;
        volatile bool stop;
        /// Whether all there is has been read once.
        public volatile bool CaughtUp;

        public Reader(Session session, string output, string error)
        {
            this.session = session;
            tails = new[] { new Tail(output, false), new Tail(error, true) };
            thread = new Thread(Run);
            thread.IsBackground = true;
            thread.Name = "vyce-monitor-reader";
        }

        public void Start()
        {
            thread.Start();
        }

        /// Of what is in the error log, the share read; 1 when there is none.
        public double Share
        {
            get { return tails[1].Length <= 0 ? 1 : (double)tails[1].Position / tails[1].Length; }
        }

        void Run()
        {
            while (!stop)
            {
                bool more = false;
                foreach (Tail tail in tails)
                {
                    try
                    {
                        more |= tail.Step(session);
                    }
                    catch (Exception e)
                    {
                        Faults.Note("leitura do log", e);
                    }
                }
                if (more) continue;
                CaughtUp = true;
                wake.WaitOne(250);
            }
        }

        public void Dispose()
        {
            stop = true;
            wake.Set();
            if (thread.IsAlive) thread.Join(2000);
            foreach (Tail tail in tails) tail.Dispose();
        }
    }

    /// The game's process: whether it runs, and how it ended.
    public sealed class Game : IDisposable
    {
        const uint Synchronize = 0x00100000, QueryLimited = 0x1000;
        IntPtr handle;
        public readonly int Pid;
        public readonly bool Opened;

        [DllImport("kernel32.dll", SetLastError = true)]
        static extern IntPtr OpenProcess(uint access, bool inherit, int pid);
        [DllImport("kernel32.dll")]
        static extern uint WaitForSingleObject(IntPtr handle, uint ms);
        [DllImport("kernel32.dll")]
        static extern bool GetExitCodeProcess(IntPtr handle, out uint code);
        [DllImport("kernel32.dll")]
        static extern bool GetProcessTimes(IntPtr handle, out long created, out long exited, out long kernel, out long user);
        [DllImport("kernel32.dll")]
        static extern bool CloseHandle(IntPtr handle);

        public Game(int pid)
        {
            Pid = pid;
            handle = OpenProcess(Synchronize | QueryLimited, false, pid);
            Opened = handle != IntPtr.Zero;
        }

        public bool Alive
        {
            get { return handle != IntPtr.Zero && WaitForSingleObject(handle, 0) == 258; }
        }

        public bool ExitCode(out uint code)
        {
            code = 0;
            return handle != IntPtr.Zero && !Alive && GetExitCodeProcess(handle, out code);
        }

        public DateTime Started { get { return Time(false); } }
        public DateTime Exited { get { return Time(true); } }

        DateTime Time(bool end)
        {
            long created, exited, kernel, user;
            if (handle == IntPtr.Zero || !GetProcessTimes(handle, out created, out exited, out kernel, out user)) return DateTime.MinValue;
            long time = end ? exited : created;
            return time <= 0 ? DateTime.MinValue : DateTime.FromFileTimeUtc(time);
        }

        public void Dispose()
        {
            if (handle != IntPtr.Zero) CloseHandle(handle);
            handle = IntPtr.Zero;
        }
    }
}
namespace Vyce.Monitor
{
    using System;
    using System.Collections.Generic;
    using System.Text;

    /// One number of the panel.
    public sealed class Tile
    {
        public string Caption, Value, Sub, Tip;
        /// Of the bar; NaN for none.
        public double Share = double.NaN;
        public Mark Mark = Mark.Info;

        public Tile(string caption, string value, string sub, string tip)
        {
            Caption = caption;
            Value = value;
            Sub = sub;
            Tip = tip;
        }
    }

    /// How the run ended, as far as the panel can tell.
    public sealed class End
    {
        public DateTime At;
        public bool Bad;
        public string Text;
        /// "código 0x887A0005"; empty if unknown or none.
        public string Code = "";

        public static End Of(Session s, bool known, uint code, DateTime at)
        {
            End end = new End { At = at };
            Hints h = s.Hints;
            bool fine = !known || code == 0;
            if (h.GpuGone != null || known && code >= 0x887A0005 && code <= 0x887A0007)
                end.Set("a placa de vídeo parou de responder" + (string.IsNullOrEmpty(h.GpuGone) ? "" : " (" + h.GpuGone + ")"), true);
            else if (h.NoMemory) end.Set("acabou a memória do Windows", true);
            else if (h.Crash != null && !(fine && known && h.WindowClosed)) end.Set("o jogo travou: " + h.Crash, true);
            else if (h.Panic && !(fine && known)) end.Set("falha interna do Vyce", true);
            else if (h.Aborted) end.Set("o jogo se encerrou por um erro interno dele", true);
            else if (fine && h.WindowClosed) end.Set("a janela foi fechada", false);
            else if (known && code != 0) end.Set(Codes.Name(code) ?? "código de saída desconhecido", true);
            else if (h.Exit != null) end.Set("o jogo saiu por conta própria", h.Exit != "0");
            else if (known) end.Set("saída normal", false);
            else end.Set("o log não diz por quê", false);
            if (known && code != 0) end.Code = "código " + Codes.Hex(code);
            return end;
        }

        void Set(string text, bool bad)
        {
            Text = text;
            Bad = bad;
        }

        public string Whole { get { return Code == "" ? Text : Text + ", " + Code; } }
    }

    /// What the window shows at one time, in words and numbers.
    public sealed class Shot
    {
        public const double Minutes = 2;
        /// Seconds without a frame after which a number is old.
        public const double Old = 4;
        public DateTime Now;
        public bool Alive;
        public string State = "", Lasted = "", Facts = "", Title = "", Foot = "";
        public Mark StateMark = Mark.Info;
        public Sample Last;
        public double Age = double.NaN;
        public bool Stale;
        public string AgeText = "", Note = "";
        public Sample[] Recent = new Sample[0];
        public double Average = double.NaN, Least = double.NaN;
        public Parts Parts;
        public string Verdict = "", GpuTop;
        public Band Band = new Band();
        public Tile[] Tiles = new Tile[0];
        public int Warnings, Errors;

        /// The picture of `s` now; the caller holds its lock.
        public static Shot Take(Session s, int pid, bool alive, End end, Machine m, DateTime now)
        {
            Shot shot = new Shot { Alive = alive };
            shot.Last = s.Last;
            shot.Now = alive || end == null ? now : end.At;
            // Of a run that is over, the last minutes there were.
            if (!alive && shot.Last != null && (shot.Now - shot.Last.At).TotalSeconds > 30) shot.Now = shot.Last.At.AddSeconds(5);
            shot.Warnings = s.Journal.Warnings;
            shot.Errors = s.Journal.Errors;
            shot.GpuTop = s.GpuTop;
            shot.Heading(s, pid, end, now);
            shot.Frames(s);
            shot.Band = Loading(s, alive, end, shot.Now);
            shot.Tiles = Numbers(s, m);
            shot.Footer(s);
            shot.Title = "Vyce | " + (shot.Last == null ? "" : alive && !shot.Stale ? Say.Fps(shot.Last.Fps) + " FPS | " : "")
                + (alive ? shot.Band.Title.Replace("  ·  ", " ") : shot.State);
            return shot;
        }

        void Heading(Session s, int pid, End end, DateTime now)
        {
            if (Alive)
            {
                State = "Jogo em execução";
                StateMark = Mark.Good;
            }
            else
            {
                State = end != null && end.Bad ? "Jogo encerrado com erro" : "Jogo encerrado";
                StateMark = end != null && end.Bad ? Mark.Error : Mark.Info;
            }
            DateTime from = s.Started > DateTime.MinValue ? s.Started : s.FirstAt, to = Alive || end == null ? now : end.At;
            if (from > DateTime.MinValue && to > from) Lasted = Say.Span((to - from).TotalSeconds);
            List<string> facts = new List<string>();
            if (s.Adapter != null) facts.Add(s.Adapter + (s.Driver == null ? "" : ", driver " + s.Driver));
            if (s.Build != null) facts.Add("versão " + s.Build);
            if (s.Console != null) facts.Add(s.Console);
            if (s.Output != null) facts.Add(s.Output);
            facts.Add("processo " + pid);
            Facts = string.Join("  ·  ", facts.ToArray());
        }

        void Frames(Session s)
        {
            DateTime from = Now.AddMinutes(-Minutes);
            int first = s.Samples.Count;
            while (first > 0 && s.Samples[first - 1].At >= from) first--;
            // One more, for the line to come in from the left edge.
            if (first > 0) first--;
            Recent = s.Samples.GetRange(first, s.Samples.Count - first).ToArray();
            double frames = 0, seconds = 0;
            foreach (Sample sample in Recent)
            {
                if (sample.At < from) continue;
                frames += sample.Frames;
                seconds += sample.Frames * sample.Ms / 1000;
                if (!(Least <= sample.Fps)) Least = sample.Fps;
            }
            if (seconds > 0) Average = frames / seconds;
            if (Last == null)
            {
                AgeText = "";
                Note = Alive ? "aguardando o primeiro quadro" : "nenhum quadro foi apresentado";
                return;
            }
            Age = Math.Max(0, (Now - Last.At).TotalSeconds);
            Stale = Alive && Age >= Old;
            AgeText = Alive ? Say.Ago(Age) : "último valor";
            if (Stale) Note = "o jogo não apresentou quadros " + Say.Ago(Age);
            else if (Say.Known(Average)) Note = "média " + Say.Fps(Average) + "  ·  menor " + Say.Fps(Least) + "  (2 min)";
            List<Sample> lately = new List<Sample>();
            for (int k = s.Samples.Count - 1; k >= 0 && (Last.At - s.Samples[k].At).TotalSeconds <= 5 && lately.Count < 8; k--)
                lately.Add(s.Samples[k]);
            Parts = Parts.Of(lately);
            if (Parts != null) Verdict = Parts.Verdict();
        }

        static string StageName(string stage)
        {
            return stage == "start" ? "Iniciando o jogo" : stage == "idle" ? "No menu ou em um vídeo" : stage == "load" ? "Carregando o mundo"
                : stage == "again" ? "Carregando outro lugar" : stage == "world" ? "No mundo" : "Abrindo o jogo";
        }

        static string Percent(double v)
        {
            return Say.Number(Math.Floor(Math.Max(0, Math.Min(100, v))), "0") + " %";
        }

        /// The loading band: from the runtime's own report, else from the frames.
        public static Band Loading(Session s, bool alive, End end, DateTime now)
        {
            Band b = new Band();
            Sample last = s.Last;
            if (!alive)
            {
                b.Title = end == null ? "Sessão encerrada" : "Como terminou: " + end.Text;
                b.Detail = (end == null || end.Code == "" ? "" : "Processo encerrado com o " + end.Code + ".  ") + "Os últimos números e os eventos continuam aqui.";
                b.Mark = end != null && end.Bad ? Mark.Error : Mark.Info;
                return b;
            }
            LoadReport r = s.Load;
            if (r == null)
            {
                FromFrames(b, s, last, now);
                return b;
            }
            double quiet = (now - r.At).TotalSeconds;
            string stage = r.Stage;
            b.Title = StageName(stage);
            if (quiet > 10)
            {
                b.Detail = "Sem notícias do carregamento " + Say.Ago(quiet) + ".";
                b.Mark = Mark.Warn;
                return b;
            }
            double percent = r["percent"], waiting = Sample.OrZero(r["pipelines_waiting"]);
            if (stage == "start" || stage == "load" || stage == "again")
            {
                if (Say.Known(percent))
                {
                    b.Title += "  ·  " + Percent(percent);
                    b.Share = Math.Max(0, Math.Min(1, percent / 100));
                }
                b.Right = Say.About(r["left_s"]);
                if (!r.Flag("known")) b.Right += b.Right == "" ? "estimativa" : " (estimativa)";
                List<string> parts = new List<string>();
                if (r["elapsed_s"] > 0) parts.Add("há " + Say.Span(r["elapsed_s"]));
                if (r["read_mb"] > 0)
                    parts.Add("lidos " + Say.Size(r["read_mb"]) + (r["read_of_mb"] > 0 ? " de " + Say.Size(Math.Max(r["read_mb"], r["read_of_mb"])) : ""));
                parts.Add(Waiting(r, waiting, s.Workers));
                b.Detail = string.Join("  ·  ", parts.ToArray());
                return b;
            }
            if (stage == "idle")
            {
                b.Detail = Say.Known(r["next_s"]) ? "Da última vez, o mundo levou " + Say.Long(r["next_s"]) + " para carregar."
                    : "Nada carregando agora.";
                return b;
            }
            World(b, s, waiting, r["pipelines_left_s"], r["pipelines_done"]);
            if (waiting < 8 && r["read_mb_s"] >= 0.5) b.Detail += "  ·  disco " + Say.Number(r["read_mb_s"], "0.0") + " MB/s";
            return b;
        }

        /// What a load waits for: the disk, pipelines or the title itself.
        static string Waiting(LoadReport r, double waiting, double workers)
        {
            double readers = Sample.OrZero(r["readers"]);
            string disk = "disco " + Say.Number(Sample.OrZero(r["read_mb_s"]), "0.0") + " MB/s";
            string busy = Say.Number(readers, "0.0") + (workers > 0 ? " de " + Say.Number(workers, "0") : "") + " leitores ocupados";
            if (waiting >= 8) return "esperando " + Say.Count(waiting) + " pipelines  ·  " + disk;
            if (readers >= 4) return "esperando o disco: " + Say.Number(Sample.OrZero(r["read_mb_s"]), "0.0") + " MB/s, " + busy;
            string queue = r["queued"] >= 1 ? "  ·  fila " + Say.Count(r["queued"]) : "";
            return (readers >= 1 ? disk + ", " + busy : "o próprio jogo está trabalhando  ·  " + disk) + queue;
        }

        static void World(Band b, Session s, double waiting, double left, double done)
        {
            if (waiting >= 8)
            {
                b.Title = "No mundo, criando " + Say.Count(waiting) + " pipelines";
                b.Right = Say.About(left);
                b.Detail = "Faltam objetos e o FPS fica baixo até terminar. Na próxima vez eles vêm do disco.";
                b.Mark = Mark.Warn;
                if (Say.Known(s.BacklogFrom) && Say.Known(done))
                {
                    double made = Math.Max(0, done - s.BacklogFrom);
                    b.Share = made / Math.Max(1, made + waiting);
                }
            }
            else b.Detail = Say.Known(s.Took) ? "Carregou em " + Say.Long(s.Took) + "." : "Nada carregando agora.";
        }

        static void FromFrames(Band b, Session s, Sample last, DateTime now)
        {
            double making = last == null ? 0 : Sample.OrZero(last.Making);
            string stage = s.Stage;
            b.Title = StageName(stage);
            if (last == null)
            {
                Row told = null;
                foreach (Row row in s.Journal.Rows)
                    if (row.Kind.Mark <= Mark.Stage) told = row;
                b.Detail = told == null ? "Aguardando o jogo." : told.Text();
                return;
            }
            string since = s.StageAt > DateTime.MinValue ? "há " + Say.Span((now - s.StageAt).TotalSeconds) : "";
            if (stage == "world")
            {
                World(b, s, making, double.NaN, double.NaN);
                return;
            }
            if (stage == "start" || stage == "load" || stage == "again")
            {
                b.Right = since;
                bool fresh = (now - last.At).TotalSeconds < Old;
                b.Detail = "Esta versão do Vyce não informa a porcentagem."
                    + (making >= 1 ? " Criando " + Say.Count(making) + (making == 1 ? " pipeline." : " pipelines.") : "")
                    + (fresh ? " O jogo segue vivo: " + Say.Fps(last.Fps) + " quadros por segundo." : " Leva alguns minutos.");
                return;
            }
            b.Detail = "Nada carregando agora." + (making >= 8 ? " Criando " + Say.Count(making) + " pipelines." : "");
        }

        static string Pair(double used, double of)
        {
            if (!Say.Known(used)) return Say.Nothing;
            return Say.Number(used / 1024, "0.0") + (of > 0 ? " / " + Say.Number(of / 1024, "0.0") : "") + " GB";
        }

        static Tile[] Numbers(Session s, Machine m)
        {
            Sample last = s.Last;
            Func<string, double> v = name => last == null ? double.NaN : last[name];
            double vram = v("vram_mb"), budget = v("vram_budget_mb"), shared = v("shared_mb"), card = v("card_mb");
            double draws = v("draws_per_frame"), dropped = v("dropped_per_frame");
            double indirect = last == null ? double.NaN : last.Indirect, making = v("pipelines_building");

            Tile video = new Tile("VRAM", Pair(vram, budget), Say.Known(shared) ? "+ " + Say.Size(shared) + " da RAM" : "",
                "Memória da placa usada pelo jogo e o limite que o Windows dá a ele.\nCheia, as texturas perdem qualidade e o jogo engasga."
                + (Say.Known(shared) ? "\nA placa também usa " + Say.Size(shared) + " da RAM." : ""));
            if (budget > 0 && Say.Known(vram))
            {
                video.Share = vram / budget;
                video.Mark = video.Share >= 1 ? Mark.Error : video.Share >= 0.9 ? Mark.Warn : Mark.Info;
            }

            Tile ram = new Tile("RAM do PC", Pair(m.RamUsed, m.RamTotal), Say.Known(m.GameRam) ? "jogo " + Say.Size(m.GameRam) : "",
                "Memória usada por todos os programas, e só pelo jogo."
                + (Say.Known(s.Commit) ? "\nO jogo reservou " + Say.Size(s.Commit) + " ao todo (RAM e arquivo de paginação)." : ""));
            if (m.RamTotal > 0 && Say.Known(m.RamUsed))
            {
                ram.Share = m.RamUsed / m.RamTotal;
                ram.Mark = ram.Share >= 0.97 ? Mark.Error : ram.Share >= 0.92 ? Mark.Warn : Mark.Info;
            }

            Tile cpu = new Tile("Processador", Say.Known(m.SystemCpu) ? Say.Number(m.SystemCpu, "0") + " %" : Say.Nothing,
                Say.Known(m.GameCpu) ? "jogo " + Say.Number(m.GameCpu, "0") + " %" : "",
                "Uso do processador inteiro, e só o do jogo."
                + (Say.Known(m.Threads) ? "\nO jogo tem " + Say.Count(m.Threads) + " threads em " + Say.Count(m.Processors) + " processadores lógicos." : ""));
            if (Say.Known(m.SystemCpu)) cpu.Share = m.SystemCpu / 100;

            double cap = s.TextureCap;
            Tile texture = new Tile("Limite de textura",
                Say.Known(cap) ? (cap > 0 ? "até " + Say.Number(cap, "0") : "sem limite") : Say.Nothing,
                !Say.Known(cap) ? "não informado" : cap > 0 && cap <= 256 ? "imagem borrada" : cap > 0 && cap <= 512 ? "menos detalhe" : "detalhe normal",
                "Maior lado das texturas novas. O Vyce diminui este limite quando a memória de vídeo enche,\ne a imagem fica borrada.");
            if (cap > 0 && cap <= 256) texture.Mark = Mark.Warn;

            Tile drawn = new Tile("Desenhos", Say.Count(draws), "por quadro",
                "Desenhos que a GPU fez em cada quadro."
                + (Say.Known(v("indirect_draws_per_frame")) ? "\nIndiretos que rodaram: " + Say.Number(v("indirect_draws_per_frame"), "0") + " desenhos e "
                    + Say.Number(v("indirect_dispatches_per_frame"), "0") + " de computação" : "")
                + (Say.Known(v("submissions_per_frame")) ? "\nEnvios à GPU por quadro: " + Say.Number(v("submissions_per_frame"), "0.0")
                    + (Say.Known(v("unwaited_per_frame")) ? ", " + Say.Number(v("unwaited_per_frame"), "0.0") + " sem espera" : "") : "")
                + (last != null ? "\nQuadros apresentados até aqui: " + Say.Count(last.Total) : ""));

            Tile lost = new Tile("Descartados", Say.Count(dropped), Say.Known(indirect) ? "+ " + Say.Count(indirect) + " indiretos" : "por quadro",
                "Desenhos que não apareceram em cada quadro: o pipeline deles ainda não existe ou a placa o recusa.\nOs indiretos são grupos que a própria GPU decidiria.");
            double all = Sample.OrZero(draws) + Sample.OrZero(dropped) + Sample.OrZero(indirect);
            if (Sample.OrZero(dropped) + Sample.OrZero(indirect) > 0.05 * all && all > 0) lost.Mark = Mark.Warn;

            Tile make = new Tile("Pipelines na fila", Say.Count(making), making >= 1 ? "sendo criados" : "nenhum agora",
                "Pipelines (shaders prontos para a placa) que ainda estão sendo criados."
                + "\nCriados nesta sessão: " + Say.Count(s.Pipelines) + ", recusados pelo driver: " + Say.Count(s.Refused) + "."
                + (Say.Known(v("pipeline_wait_ms")) ? "\nEspera por eles: " + Say.Ms(v("pipeline_wait_ms")) + " por quadro." : ""));
            if (making >= 8) make.Mark = Mark.Warn;

            Tile buffers = new Tile("Buffers na VRAM", Say.Size(card), "dados do jogo",
                "Dados do jogo mantidos na memória da placa em vez da RAM."
                + (Say.Known(v("host_tex_mb")) ? "\nTexturas na placa: " + Say.Size(v("host_tex_mb")) + "." : "")
                + Fields(last, "card_"));
            return new[] { video, ram, cpu, texture, drawn, lost, make, buffers };
        }

        /// The numbers of a sample whose names start alike, as they are.
        static string Fields(Sample last, string start)
        {
            if (last == null) return "";
            StringBuilder text = new StringBuilder();
            foreach (string name in Sample.Names())
            {
                if (!name.StartsWith(start, StringComparison.Ordinal) || name == "card_mb" || !Say.Known(last[name])) continue;
                text.Append("\n").Append(name).Append(": ").Append(Say.Number(last[name], "0.##"));
            }
            return text.ToString();
        }

        void Footer(Session s)
        {
            List<string> parts = new List<string>();
            if (s.Shaders > 0) parts.Add("shaders " + Say.Count(s.Shaders));
            if (s.Pipelines > 0) parts.Add("pipelines " + Say.Count(s.Pipelines));
            if (s.Refused > 0) parts.Add("recusados " + Say.Count(s.Refused));
            if (s.Files > 0) parts.Add("arquivos " + Say.Count(s.Files));
            if (s.LastLineAt > DateTime.MinValue) parts.Add("última linha do log às " + Say.Clock(s.LastLineAt));
            Foot = string.Join("  ·  ", parts.ToArray());
        }
    }

    /// The text for pasting into a chat.
    public static class Summary
    {
        static double Median(List<double> values)
        {
            if (values.Count == 0) return double.NaN;
            values.Sort();
            int half = values.Count / 2;
            return values.Count % 2 == 1 ? values[half] : (values[half - 1] + values[half]) / 2;
        }

        /// The median of a number over the last minute.
        static double Of(List<Sample> samples, string name)
        {
            List<double> values = new List<double>();
            foreach (Sample s in samples)
                if (Say.Known(s[name])) values.Add(s[name]);
            return Median(values);
        }

        static void Part(List<string> parts, string text, double value, Func<double, string> say)
        {
            if (Say.Known(value)) parts.Add(text + " " + say(value));
        }

        /// The caller holds the session's lock.
        public static string Text(Session s, Shot shot, Machine m, string errorLog)
        {
            StringBuilder t = new StringBuilder();
            t.AppendLine("Vyce, resumo da sessão em " + shot.Now.ToLocalTime().ToString("dd/MM/yyyy HH:mm:ss"));
            t.AppendLine("Estado: " + shot.State + (shot.Lasted == "" ? "" : ", duração " + shot.Lasted));
            t.AppendLine((shot.Alive ? "Etapa: " : "") + shot.Band.Title + (shot.Band.Right == "" ? "" : ", " + shot.Band.Right)
                + (Say.Known(s.Took) ? "; o mundo carregou em " + Say.Long(s.Took) : "") + (shot.Alive ? "" : ". " + shot.Band.Detail.Split('.')[0]));
            if (shot.Facts != "") t.AppendLine("PC: " + shot.Facts.Replace("  ·  ", "; "));
            if (s.BuildHash != null) t.AppendLine("Versão do jogo: " + s.Build + " (" + Say.Cut(s.BuildHash, 17) + ")");
            Sample last = s.Last;
            if (last == null) t.AppendLine("Nenhum quadro foi apresentado.");
            else Numbers(t, s, last, shot);
            List<string> pc = new List<string>();
            Part(pc, "RAM", m.RamUsed, v => Say.Size(v) + " de " + Say.Size(m.RamTotal));
            Part(pc, "RAM do jogo", m.GameRam, Say.Size);
            Part(pc, "CPU", m.SystemCpu, v => Say.Number(v, "0") + " %");
            Part(pc, "CPU do jogo", m.GameCpu, v => Say.Number(v, "0") + " %");
            Part(pc, "reservado pelo jogo", s.Commit, Say.Size);
            if (pc.Count > 0) t.AppendLine("Agora: " + string.Join("; ", pc.ToArray()));
            t.AppendLine("Contagens: shaders " + Say.Count(s.Shaders) + "; pipelines " + Say.Count(s.Pipelines) + "; recusados " + Say.Count(s.Refused)
                + "; arquivos " + Say.Count(s.Files) + "; avisos " + Say.Count(s.Journal.Warnings) + "; erros " + Say.Count(s.Journal.Errors));
            Counts(t, s, Mark.Error, "Erros por tipo");
            Counts(t, s, Mark.Warn, "Avisos por tipo");
            List<Row> errors = s.Journal.Rows.FindAll(r => r.Kind.Mark == Mark.Error);
            if (errors.Count > 0)
            {
                t.AppendLine("Últimos erros:");
                for (int k = Math.Max(0, errors.Count - 4); k < errors.Count; k++)
                {
                    Row row = errors[k];
                    t.AppendLine("  " + Say.Clock(row.Last) + " " + row.Text());
                    if (row.Kept.Count > 0) t.AppendLine("    " + Say.Cut(row.Kept[row.Kept.Count - 1], 260));
                }
            }
            if (s.BadTelemetry > 0) t.AppendLine("Linhas de telemetria ilegíveis: " + s.BadTelemetry);
            if (Faults.Count > 0) t.AppendLine("Falhas do próprio painel: " + Faults.Count + " (" + Faults.Last + ")");
            t.Append("Log: " + errorLog);
            return t.ToString();
        }

        static void Numbers(StringBuilder t, Session s, Sample last, Shot shot)
        {
            List<Sample> minute = new List<Sample>();
            foreach (Sample sample in s.Samples)
                if ((last.At - sample.At).TotalSeconds <= 60) minute.Add(sample);
            Func<string, double> of = name => Of(minute, name);
            double ms = of("frame_ms");
            t.AppendLine("Medianas dos últimos 60 s (" + minute.Count + " amostras, a última " + (shot.Alive ? Say.Ago(shot.Age) : "antes do fim") + "): "
                + Say.Fps(of("fps")) + " FPS, " + Say.Ms(ms) + " por quadro");
            double gpu = of("gpu_wait_ms"), prepare = of("cpu_prepare_ms"), back = of("copy_back_ms");
            Func<double, string> part = v => Say.Ms(v) + (ms > 0 ? " (" + Say.Share(v / ms) + ")" : "");
            List<string> parts = new List<string>();
            Part(parts, "espera da GPU", gpu, part);
            Part(parts, "GPU ocupada", of("gpu_busy_ms"), part);
            Part(parts, "preparo na CPU", prepare, part);
            Part(parts, "cópia de volta", back, part);
            Part(parts, "resto", ms - gpu - prepare - back, v => part(Math.Max(0, v)));
            t.AppendLine("  " + string.Join("; ", parts.ToArray()) + ". " + shot.Verdict + ".");
            parts.Clear();
            Part(parts, "cópias", of("cpu_copy_ms"), v => Say.Ms(v) + " " + Say.Size(of("cpu_copy_mb")));
            Part(parts, "texturas", of("texture_refresh_ms"), v => Say.Ms(v) + " " + Say.Size(of("texture_refresh_mb")));
            Part(parts, "descritores", of("bindless_plan_ms"), Say.Ms);
            Part(parts, "cópias de referência", of("snapshot_ms"), Say.Ms);
            Part(parts, "trabalho adiado", of("steps_ms"), Say.Ms);
            Part(parts, "envio", of("submit_ms"), Say.Ms);
            Part(parts, "espera por pipelines", of("pipeline_wait_ms"), Say.Ms);
            if (parts.Count > 0) t.AppendLine("  Preparo: " + string.Join("; ", parts.ToArray()));
            parts.Clear();
            Part(parts, "desenhos", of("draws_per_frame"), Say.Count);
            Part(parts, "descartados", of("dropped_per_frame"), Say.Count);
            List<double> indirect = new List<double>();
            foreach (Sample sample in minute)
                if (Say.Known(sample.Indirect)) indirect.Add(sample.Indirect);
            Part(parts, "indiretos descartados", Median(indirect), Say.Count);
            Part(parts, "envios", of("submissions_per_frame"), v => Say.Number(v, "0.0") + " (" + Say.Number(of("unwaited_per_frame"), "0.0") + " sem espera)");
            Part(parts, "pipelines na fila", of("pipelines_building"), Say.Count);
            t.AppendLine("  " + string.Join("; ", parts.ToArray()));
            parts.Clear();
            Part(parts, "VRAM", of("vram_mb"), v => Say.Size(v) + " de " + Say.Size(of("vram_budget_mb")));
            Part(parts, "compartilhada", of("shared_mb"), Say.Size);
            Part(parts, "buffers na placa", of("card_mb"), Say.Size);
            Part(parts, "texturas na placa", of("host_tex_mb"), Say.Size);
            Part(parts, "limite de textura", s.TextureCap, v => Say.Number(v, "0"));
            if (parts.Count > 0) t.AppendLine("  " + string.Join("; ", parts.ToArray()));
            if (s.GpuTop != null) t.AppendLine("  Mais pesado na GPU: " + s.GpuTop);
            // What this summary does not know by name, as the runtime calls it.
            List<string> more = new List<string>();
            foreach (string name in Sample.Names())
            {
                if (Told.Contains(name)) continue;
                double value = of(name);
                if (Say.Known(value)) more.Add(name + "=" + value.ToString("0.##", System.Globalization.CultureInfo.InvariantCulture));
            }
            if (more.Count > 0) t.AppendLine("  Outros campos (medianas): " + string.Join(" ", more.ToArray()));
        }

        static readonly HashSet<string> Told = new HashSet<string>(new[]
        {
            "schema", "fps", "frame_ms", "frames_total", "gpu_wait_ms", "gpu_busy_ms", "cpu_prepare_ms", "copy_back_ms",
            "cpu_copy_ms", "cpu_copy_mb", "texture_refresh_ms", "texture_refresh_mb", "bindless_plan_ms", "snapshot_ms",
            "steps_ms", "submit_ms", "pipeline_wait_ms", "draws_per_frame", "dropped_per_frame", "indirect_per_frame",
            "indirect_dropped_per_frame", "submissions_per_frame", "unwaited_per_frame", "pipelines_building", "vram_mb",
            "vram_budget_mb", "shared_mb", "card_mb", "host_tex_mb", "t_s", "tex_cap",
        }, StringComparer.Ordinal);

        static void Counts(StringBuilder t, Session s, Mark mark, string title)
        {
            List<Journal.Total> totals = new List<Journal.Total>();
            foreach (Journal.Total total in s.Journal.Totals.Values)
                if (total.Kind.Mark == mark) totals.Add(total);
            if (totals.Count == 0) return;
            totals.Sort((a, b) => b.Count.CompareTo(a.Count));
            List<string> parts = new List<string>();
            foreach (Journal.Total total in totals)
            {
                if (parts.Count >= 14) break;
                parts.Add(Say.Count(total.Count) + " " + total.Kind.Label);
            }
            t.AppendLine(title + ": " + string.Join("; ", parts.ToArray()));
        }
    }
}
'@
