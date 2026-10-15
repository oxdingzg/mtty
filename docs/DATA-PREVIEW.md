# Data preview / 数据预览

Open a buffered JSON, CSV/TSV or text/log file and run **Preview JSON / CSV / Log…**.
The preview uses the current editor text, including unsaved edits, and labels its
snapshot revision. **Refresh buffer** captures the latest text.

- **JSON**: expandable tree, key/value filtering, exact large-number handling,
  selected-value copying, JSON Pointer copying and Composer context sharing.
- **CSV/TSV**: quoted delimiters, doubled quotes, multiline fields and CRLF;
  optional first-row headers, case-insensitive row filtering and text sorting.
  Select a cell to copy its original value or share it with Composer.
- **Lines**: line-oriented logs use the same filtered table. **Go to source record**
  locates the original line in the editor. CSV records retain their starting source
  line even when a quoted field contains multiple lines. A changed buffer requires
  refreshing before navigation.

Parsing runs on a worker; rows are virtualized. Preview input is limited to 2 MiB,
JSON scans to 20,000 nodes / 10,000 visible rows, and tables to 10,000 records /
128 columns. The window identifies clipped previews. Source buffers remain editable
through the editor, and preview operations do not write files.

打开 JSON、CSV/TSV 或文本日志缓冲区，运行 **预览 JSON / CSV / 日志…**。
预览读取包含未保存编辑的当前文本，并显示快照版本；“刷新缓冲区”获取最新文本。

JSON 支持展开、按键和值筛选、精确大整数、复制选中值和 JSON Pointer。
CSV/TSV 支持引号、转义引号、字段内换行、CRLF、表头、筛选及文本排序。
日志按行展示；表格和日志可定位回原始记录，筛选和排序后仍保留源行号。
选中数据可复制或放入 Composer。大数据采用明确上限和虚拟化行展示。
