# ADR 0011 — 标签分组、文件树与 Recipes

> English (default): [`0011-tab-groups-tree-recipes.md`](0011-tab-groups-tree-recipes.md)

状态:已接受。

## 背景

roadmap 的 U1 要求更丰富的标签(标记/分隔符/分组)与可浏览的侧栏;U7 要求保存与
回放工作区(*recipes*)以及导出配置。这些都是在既有 tab/session 模型之上、局部的小改动。

## 决定

**标签外观(U1)。** 标签新增两个可选字段 —— `mark`(标题后)与 `group`。顶栏在分组
变化处画出竖直分隔符,使同组标签相邻。两者都从侧栏右键菜单设置(Rename / Mark /
Group,以及 *Remove from Group*),共用一个以 `TabField` 为键的对话框;并作为
`#[serde(default)]` 字段随 `session.json` 往返(旧会话照常加载)。
状态仍由标签图标表示;标题标注分组(`[work] 标题★`),窄标签也仍能看出它属于哪一组。侧栏则以分节呈现:每组上方标出组名,行内不再重复前缀,顶栏保留带前缀的标题。加入分组时标签会移到同组最后一个标签旁边;分组对话框列出已有分组,加入只需
点选,不必重新输入名称。

**文件树(U1)。** 左栏在标签列表下方显示焦点 pane 的 cwd 树。目录可展开/折叠
(状态在 `tree_expanded: HashSet<PathBuf>`);双击文件用 ADR 0009 的查看/编辑器打开。
为控制开销,跳过隐藏项、每个目录最多 200 项、递归深度上限 6。

**Recipes(U7)。** 整个工作区(标签、分屏、cwd —— 即既有 `Session`)以 JSON 保存到
`~/.config/miaotty/recipes/<name>.json`,并用启动时同一个 `restore` 路径恢复。面板提供
*Save Recipe…* 与 *Open Recipe…*;名称会清理为安全文件名。配置导出即既有的
*Save to config.toml* 加 *Save views.json*。

## 后果

- 分组/标记是纯展示,且带默认值序列化,无需迁移。
- Recipes 复用会话格式,故凡是启动能恢复的都能保存与回放。
- 文件树在绘制时直接读取目录;各项上限使其开销很小,日后可用缓存/异步加载替换而不改 UI。
