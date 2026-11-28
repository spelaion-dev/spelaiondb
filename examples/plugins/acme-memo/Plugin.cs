// Acme Memo: an example of a plugin someone else writes for SpelaionDB, in C#, following
// docs/plugin-authors.md. Its engine keeps a few notes in memory: it connects, lists one schema
// and one table, describes it, and answers any SELECT with its rows. It is the wire only:
// SpelaionDB splits scripts, holds the password and decides what runs.
//
// It needs nothing but the .NET Framework every Windows has (C# 5, `build.ps1`). Everything the
// protocol needs is here: frames on standard input and output, JSON-RPC messages, and Arrow IPC
// for rows, written by hand below (the few flatbuffers tables a result of text columns needs).

using System;
using System.Collections.Generic;
using System.IO;
using System.Text;
using System.Web.Script.Serialization;

static class Plugin
{
    const string Engine = "acme-memo";
    const string Name = "acme-memo";
    const string Version = "1.0.0";

    static readonly string[][] Notes =
    {
        new[] { "1", "Buy milk" },
        new[] { "2", "Call the bank" },
        new[] { "3", null },
    };

    static Stream input;
    static Stream output;
    static readonly JavaScriptSerializer Json = new JavaScriptSerializer();
    static readonly HashSet<long> Sessions = new HashSet<long>();

    static void Main()
    {
        input = Console.OpenStandardInput();
        output = Console.OpenStandardOutput();
        while (true)
        {
            byte kind;
            byte[] payload;
            // Standard input closed: SpelaionDB wants this plugin to end. Nothing is kept open.
            if (!ReadFrame(out kind, out payload)) return;
            // Data frames only come with `insert_rows`, which this engine does not do.
            if (kind != 1) continue;
            var message = Json.DeserializeObject(Encoding.UTF8.GetString(payload)) as Dictionary<string, object>;
            if (message == null) continue;
            object id;
            // A notification (stream.credit, stream.drop) needs no answer here: every result is
            // one batch, sent whole.
            if (!message.TryGetValue("id", out id) || id == null) continue;
            var method = message["method"] as string;
            object rawParams;
            message.TryGetValue("params", out rawParams);
            var p = rawParams as Dictionary<string, object> ?? new Dictionary<string, object>();
            try
            {
                Handle(id, method, p);
            }
            catch (Exception e)
            {
                Log("failed: " + e.Message);
                Fail(id, Other(e.Message));
            }
        }
    }

    static void Handle(object id, string method, Dictionary<string, object> p)
    {
        switch (method)
        {
            case "initialize":
                Reply(id, Obj(
                    "protocol", Obj("major", 1, "minor", 0),
                    "plugin", Obj("name", Name, "version", Version),
                    "engines", new[] { Engine },
                    "credit", 8));
                return;
            case "connect":
            {
                // The password arrives here and nowhere else; this engine needs none.
                var database = p.ContainsKey("database") ? p["database"] as string : "";
                if (database == "refuse")
                {
                    Fail(id, Obj("kind", "connection", "message", "this memo has no database called refuse"));
                    return;
                }
                Sessions.Add(Convert.ToInt64(p["session"]));
                Reply(id, Obj("capabilities", Obj(
                    "transactions", false, "cancel", false, "explain", false,
                    "editable_results", false, "schemas", true, "insert_rows", false,
                    "bound_params", false, "insert_bytes", false)));
                return;
            }
            case "close":
                Sessions.Remove(Convert.ToInt64(p["session"]));
                Reply(id, Obj());
                return;
            case "cancel":
                // Every statement here ends before the next message is read.
                Reply(id, Obj());
                return;
            case "server_version":
                Reply(id, Obj("version", "Acme Memo " + Version));
                return;
            case "keep_alive":
                Reply(id, Obj("state", "alive"));
                return;
            case "transaction_state":
                Reply(id, Obj("state", "idle"));
                return;
            case "current_database":
            case "database_context":
                Reply(id, Obj("database", null));
                return;
            case "list_schemas":
                Reply(id, Obj("schemas", new[] { Obj("name", "main", "system", false) }));
                return;
            case "list_objects":
                Reply(id, Obj("objects", (string)p["schema"] == "main" ? new[] { NotesObject() } : new object[0]));
                return;
            case "describe_table":
                if ((string)p["schema"] != "main" || (string)p["name"] != "notes")
                {
                    Fail(id, Obj("kind", "database", "message", "no such table", "code", null,
                        "detail", null, "hint", null, "position", null));
                    return;
                }
                Reply(id, Obj("table", Obj(
                    "object", NotesObject(),
                    "columns", new[] { Column("id", "integer", false), Column("body", "text", true) },
                    "primary_key", new[] { "id" },
                    "indexes", new object[0],
                    "foreign_keys", new object[0],
                    "constraints", new object[0])));
                return;
            case "list_columns":
                Reply(id, Obj("columns", (string)p["schema"] == "main"
                    ? new[] { Obj("table", "notes", "name", "id", "native_type", "integer"), Obj("table", "notes", "name", "body", "native_type", "text") }
                    : new object[0]));
                return;
            case "list_schema_objects":
            case "list_server_objects":
                Reply(id, Obj("objects", new object[0]));
                return;
            case "list_referencing_keys":
                Reply(id, Obj("keys", new object[0]));
                return;
            case "execute":
                Execute(id, p);
                return;
            default:
                // An unknown method: JSON-RPC's answer, never silence.
                Write(1, Encoding.UTF8.GetBytes(Json.Serialize(Obj("jsonrpc", "2.0", "id", id,
                    "error", Obj("code", -32601, "message", "this plugin does not know " + method)))));
                return;
        }
    }

    /// <summary>One statement: the answer at once, then its result on the stream, as Arrow.</summary>
    static void Execute(object id, Dictionary<string, object> p)
    {
        long stream = Convert.ToInt64(p["stream"]);
        var sql = (p["sql"] as string ?? "").TrimStart();
        Reply(id, Obj());
        if (!sql.StartsWith("select", StringComparison.OrdinalIgnoreCase))
        {
            Notify("stream.error", Obj("stream", stream, "error", Obj("kind", "database",
                "message", "Acme Memo only answers SELECT", "code", null, "detail", null,
                "hint", null, "position", null)));
        }
        else
        {
            var columns = new[] { new ArrowColumn("id", "integer"), new ArrowColumn("body", "text") };
            Data(stream, true, Arrow.SchemaMessage(columns));
            Data(stream, false, Arrow.BatchMessage(Notes));
            Notify("stream.done", Obj("stream", stream, "rows_affected", null));
        }
        Notify("stream.end", Obj("stream", stream));
    }

    static Dictionary<string, object> NotesObject()
    {
        return Obj("schema", "main", "name", "notes", "kind", "table",
            "estimated_rows", Notes.Length, "comment", "Notes kept in memory");
    }

    static Dictionary<string, object> Column(string name, string type, bool nullable)
    {
        return Obj("name", name, "native_type", type, "nullable", nullable, "default", null, "comment", null);
    }

    static Dictionary<string, object> Other(string message)
    {
        return Obj("kind", "other", "message", message);
    }

    static Dictionary<string, object> Obj(params object[] pairs)
    {
        var map = new Dictionary<string, object>();
        for (int i = 0; i < pairs.Length; i += 2) map[(string)pairs[i]] = pairs[i + 1];
        return map;
    }

    static void Reply(object id, object result)
    {
        Write(1, Encoding.UTF8.GetBytes(Json.Serialize(Obj("jsonrpc", "2.0", "id", id, "result", result))));
    }

    static void Fail(object id, Dictionary<string, object> error)
    {
        Write(1, Encoding.UTF8.GetBytes(Json.Serialize(Obj("jsonrpc", "2.0", "id", id,
            "error", Obj("code", -32000, "message", error["message"], "data", error)))));
    }

    static void Notify(string method, object parameters)
    {
        Write(1, Encoding.UTF8.GetBytes(Json.Serialize(Obj("jsonrpc", "2.0", "method", method, "params", parameters))));
    }

    /// <summary>A data frame: the stream's id, a flag (1: a new IPC stream starts), its bytes.</summary>
    static void Data(long stream, bool starts, byte[] ipc)
    {
        var payload = new byte[9 + ipc.Length];
        BitConverter.GetBytes(stream).CopyTo(payload, 0);
        payload[8] = (byte)(starts ? 1 : 0);
        ipc.CopyTo(payload, 9);
        Write(2, payload);
    }

    /// <summary>A frame: its length (the kind byte and the payload), the kind, the payload.</summary>
    static void Write(byte kind, byte[] payload)
    {
        output.Write(BitConverter.GetBytes(payload.Length + 1), 0, 4);
        output.WriteByte(kind);
        output.Write(payload, 0, payload.Length);
        output.Flush();
    }

    static bool ReadFrame(out byte kind, out byte[] payload)
    {
        kind = 0;
        payload = null;
        var length = ReadExactly(4);
        if (length == null) return false;
        int size = BitConverter.ToInt32(length, 0);
        if (size < 1 || size > (64 << 20)) return false;
        var rest = ReadExactly(size);
        if (rest == null) return false;
        kind = rest[0];
        payload = new byte[size - 1];
        Array.Copy(rest, 1, payload, 0, size - 1);
        return true;
    }

    static byte[] ReadExactly(int count)
    {
        var buffer = new byte[count];
        int read = 0;
        while (read < count)
        {
            int n = input.Read(buffer, read, count - read);
            if (n <= 0) return null;
            read += n;
        }
        return buffer;
    }

    /// <summary>Standard error is the plugin's log; standard output is the protocol only.</summary>
    static void Log(string line)
    {
        Console.Error.WriteLine(line);
    }
}

sealed class ArrowColumn
{
    public readonly string Name;
    public readonly string NativeType;

    public ArrowColumn(string name, string nativeType)
    {
        Name = name;
        NativeType = nativeType;
    }
}

/// <summary>
/// The Arrow IPC messages of a result whose columns are all text (`Utf8`): a schema, then a record
/// batch. Each field says `spelaion.logical = text` and its engine's native type.
/// </summary>
static class Arrow
{
    const short MetadataV5 = 4;
    const byte HeaderSchema = 1;
    const byte HeaderRecordBatch = 3;
    const byte TypeUtf8 = 5;

    public static byte[] SchemaMessage(ArrowColumn[] columns)
    {
        var fields = new List<Table>();
        foreach (var column in columns)
        {
            var metadata = new List<Table>
            {
                KeyValue("spelaion.logical", "text"),
                KeyValue("spelaion.native_type", column.NativeType),
            };
            fields.Add(new Table()
                .Ref(0, new Str(column.Name))
                .Byte(1, 1) // nullable
                .Byte(2, TypeUtf8) // type's kind
                .Ref(3, new Table()) // Utf8 has no fields
                .Ref(5, new TableVector(new List<Table>())) // children
                .Ref(6, new TableVector(metadata)));
        }
        var schema = new Table()
            .Short(0, 0) // little-endian
            .Ref(1, new TableVector(fields));
        return Encapsulate(Message(HeaderSchema, schema, 0), new byte[0]);
    }

    public static byte[] BatchMessage(string[][] rows)
    {
        int width = rows.Length == 0 ? 0 : rows[0].Length;
        var body = new MemoryStream();
        var nodes = new MemoryStream();
        var buffers = new MemoryStream();
        for (int c = 0; c < width; c++)
        {
            var validity = new byte[(rows.Length + 7) / 8];
            var offsets = new MemoryStream();
            var data = new MemoryStream();
            int nulls = 0;
            offsets.Write(BitConverter.GetBytes(0), 0, 4);
            for (int r = 0; r < rows.Length; r++)
            {
                var value = rows[r][c];
                if (value == null)
                {
                    nulls++;
                }
                else
                {
                    validity[r / 8] |= (byte)(1 << (r % 8));
                    var bytes = Encoding.UTF8.GetBytes(value);
                    data.Write(bytes, 0, bytes.Length);
                }
                offsets.Write(BitConverter.GetBytes((int)data.Length), 0, 4);
            }
            Long(nodes, rows.Length);
            Long(nodes, nulls);
            Buffer(body, buffers, nulls == 0 ? new byte[0] : validity);
            Buffer(body, buffers, offsets.ToArray());
            Buffer(body, buffers, data.ToArray());
        }
        var batch = new Table()
            .Long(0, rows.Length)
            .Ref(1, new StructVector(nodes.ToArray(), width, 8))
            .Ref(2, new StructVector(buffers.ToArray(), width * 3, 8));
        var bodyBytes = body.ToArray();
        return Encapsulate(Message(HeaderRecordBatch, batch, bodyBytes.Length), bodyBytes);
    }

    static Table KeyValue(string key, string value)
    {
        return new Table().Ref(0, new Str(key)).Ref(1, new Str(value));
    }

    static Table Message(byte header, Table content, long bodyLength)
    {
        return new Table()
            .Short(0, MetadataV5)
            .Byte(1, header)
            .Ref(2, content)
            .Long(3, bodyLength);
    }

    /// <summary>A buffer of the body, padded to 8 bytes, and its place in the batch's list.</summary>
    static void Buffer(MemoryStream body, MemoryStream list, byte[] bytes)
    {
        Long(list, body.Length);
        Long(list, bytes.Length);
        body.Write(bytes, 0, bytes.Length);
        while (body.Length % 8 != 0) body.WriteByte(0);
    }

    static void Long(MemoryStream stream, long value)
    {
        stream.Write(BitConverter.GetBytes(value), 0, 8);
    }

    /// <summary>An encapsulated IPC message: the continuation marker, the metadata's length
    /// (padded so the body starts on 8 bytes), the metadata, the body.</summary>
    static byte[] Encapsulate(Table message, byte[] body)
    {
        var metadata = FlatBuffer.Write(message);
        var stream = new MemoryStream();
        stream.Write(BitConverter.GetBytes(-1), 0, 4);
        stream.Write(BitConverter.GetBytes(metadata.Length), 0, 4);
        stream.Write(metadata, 0, metadata.Length);
        stream.Write(body, 0, body.Length);
        return stream.ToArray();
    }
}

/// <summary>Something a flatbuffer table refers to.</summary>
abstract class Node
{
}

sealed class Str : Node
{
    public readonly string Value;

    public Str(string value)
    {
        Value = value;
    }
}

sealed class TableVector : Node
{
    public readonly List<Table> Items;

    public TableVector(List<Table> items)
    {
        Items = items;
    }
}

sealed class StructVector : Node
{
    public readonly byte[] Bytes;
    public readonly int Count;
    public readonly int Align;

    public StructVector(byte[] bytes, int count, int align)
    {
        Bytes = bytes;
        Count = count;
        Align = align;
    }
}

/// <summary>A flatbuffer table: its fields by slot, each a scalar or a reference.</summary>
sealed class Table : Node
{
    public sealed class Field
    {
        public int Slot;
        public byte[] Scalar;
        public Node Reference;
        public int Offset;

        public int Size
        {
            get { return Scalar != null ? Scalar.Length : 4; }
        }
    }

    public readonly List<Field> Fields = new List<Field>();

    public Table Byte(int slot, byte value)
    {
        Fields.Add(new Field { Slot = slot, Scalar = new[] { value } });
        return this;
    }

    public Table Short(int slot, short value)
    {
        Fields.Add(new Field { Slot = slot, Scalar = BitConverter.GetBytes(value) });
        return this;
    }

    public Table Long(int slot, long value)
    {
        Fields.Add(new Field { Slot = slot, Scalar = BitConverter.GetBytes(value) });
        return this;
    }

    public Table Ref(int slot, Node node)
    {
        Fields.Add(new Field { Slot = slot, Reference = node });
        return this;
    }
}

/// <summary>
/// Writes a flatbuffer front to back: the root offset, then each table with its vtable just before
/// it, then what it refers to after it (a reference always points forward). Every scalar is
/// aligned to its size from the buffer's start, as readers check.
/// </summary>
sealed class FlatBuffer
{
    readonly List<byte> bytes = new List<byte>();

    public static byte[] Write(Table root)
    {
        var writer = new FlatBuffer();
        writer.Put(new byte[4]);
        int at = writer.Node(root);
        writer.Patch(0, at);
        writer.Align(8);
        return writer.bytes.ToArray();
    }

    int Position
    {
        get { return bytes.Count; }
    }

    void Align(int n)
    {
        while (bytes.Count % n != 0) bytes.Add(0);
    }

    void Put(byte[] value)
    {
        bytes.AddRange(value);
    }

    /// <summary>The offset at `at` points to `target`, after it.</summary>
    void Patch(int at, int target)
    {
        var value = BitConverter.GetBytes(target - at);
        for (int i = 0; i < 4; i++) bytes[at + i] = value[i];
    }

    int Node(Node node)
    {
        var text = node as Str;
        if (text != null) return String(text);
        var tables = node as TableVector;
        if (tables != null) return Tables(tables);
        var structs = node as StructVector;
        if (structs != null) return Structs(structs);
        return Table((Table)node);
    }

    int String(Str text)
    {
        Align(4);
        int at = Position;
        var value = Encoding.UTF8.GetBytes(text.Value);
        Put(BitConverter.GetBytes(value.Length));
        Put(value);
        bytes.Add(0);
        return at;
    }

    int Tables(TableVector vector)
    {
        Align(4);
        int at = Position;
        Put(BitConverter.GetBytes(vector.Items.Count));
        int first = Position;
        foreach (var unused in vector.Items) Put(new byte[4]);
        for (int i = 0; i < vector.Items.Count; i++) Patch(first + 4 * i, Table(vector.Items[i]));
        return at;
    }

    int Structs(StructVector vector)
    {
        // The elements, right after the 4-byte count, are aligned to the struct's alignment.
        while ((Position + 4) % vector.Align != 0) bytes.Add(0);
        int at = Position;
        Put(BitConverter.GetBytes(vector.Count));
        Put(vector.Bytes);
        return at;
    }

    int Table(Table table)
    {
        // The fields after the 4-byte offset to the vtable, the largest first, each aligned.
        var fields = new List<Table.Field>(table.Fields);
        fields.Sort((a, b) => b.Size.CompareTo(a.Size));
        int size = 4;
        int align = 4;
        int slots = 0;
        foreach (var field in fields)
        {
            while (size % field.Size != 0) size++;
            field.Offset = size;
            size += field.Size;
            align = Math.Max(align, field.Size);
            slots = Math.Max(slots, field.Slot + 1);
        }
        // The vtable: its size, the table's size, each slot's offset (0 for an absent one).
        Align(2);
        int vtable = Position;
        Put(BitConverter.GetBytes((short)(4 + 2 * slots)));
        Put(BitConverter.GetBytes((short)size));
        for (int slot = 0; slot < slots; slot++)
        {
            var found = fields.Find(f => f.Slot == slot);
            Put(BitConverter.GetBytes((short)(found == null ? 0 : found.Offset)));
        }
        Align(align);
        int at = Position;
        Put(BitConverter.GetBytes(at - vtable));
        var body = new byte[size - 4];
        foreach (var field in fields)
        {
            if (field.Scalar != null) field.Scalar.CopyTo(body, field.Offset - 4);
        }
        Put(body);
        foreach (var field in fields)
        {
            if (field.Reference != null) Patch(at + field.Offset, Node(field.Reference));
        }
        return at;
    }
}
