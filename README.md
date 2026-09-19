# WinBeautify

Windows 桌面美化与增强工具。任务栏材质、任务栏歌词与实时频谱、剪贴板历史、任务清单，
全部跑在一个 Rust 进程里，空闲时不做任何轮询。

参考了 TranslucentTB、Rainmeter、PowerToys、ModernFlyouts 的做法，用 Rust + Tauri 2 实现。

---

## 功能

| 模块 | 内容 |
|---|---|
| **任务栏** | 正常 / 透明 / 模糊 / 亚克力 / 纯色，顶部细线开关，副屏任务栏，动态模式（窗口最大化时切换效果），全屏应用自动暂停，退出时还原 |
| **小组件栏** | 嵌入任务栏的 Widget Bar：启动器 + 自适应宽度的音频组件；前景色可跟随主题或自定义 |
| **任务栏歌词** | 通过 GSMTC 读取当前曲目，LRC 解析、可选在线接口（结果只存内存），歌词偏移补偿 |
| **音乐频谱** | WASAPI 回环捕获系统音频 + rustfft，对数分频，无需虚拟声卡；柱状与上下律动两种样式 |
| **媒体控制** | 指针悬停时显示上一首 / 播放暂停 / 下一首 |
| **剪贴板** | 事件驱动监听，文本 / 图片 / 文件列表，搜索、收藏（★）、去重、容量上限；图片可一键贴到屏幕 |
| **任务清单** | SQLite 存储，原生面板内增删改查（勾选、就地改名、删除），Markdown / JSON 导出 |
| **截图贴图** | F1 拖选截屏（十字线 + 8 倍放大镜 + 坐标/取色），直接写入剪贴板（CF_DIB）；F3 把剪贴板的图片贴到屏幕最上层，可拖动、可缩放、带关闭按钮 |
| **设置中心** | 全部配置的图形界面，改动即时生效并写入 `config.toml`；原生 Direct2D 窗口 |

---

## 快速开始

```bash
# beautify-taskbar-tap 必须一起构建：它是注入 explorer 的 TAP DLL
# （beautify_taskbar_tap.dll），宿主在 exe 同目录找它，缺了任务栏透明
# 就只能退回 22H2 之前的旧路径。
cargo build --release -p winbeautify -p beautify-taskbar-tap
./target/release/winbeautify.exe
```

启动后常驻托盘，左键点托盘图标打开设置，右键菜单可以打开 Flyout、重载配置或退出。

---

## 架构

```
winbeautify/
├── crates/
│   ├── beautify-core/       配置 schema、事件总线、模块契约、数据模型
│   ├── beautify-taskbar/    任务栏材质（DWM + 未公开合成 API）、Shell 几何
│   ├── beautify-media/      GSMTC 会话、LRC、WinHTTP、WASAPI 回环 + rustfft
│   ├── beautify-clipboard/  剪贴板监听、DIB→BMP、SQLite 历史
│   ├── beautify-todo/       任务存储、导出
│   ├── beautify-snip/       截图：抓屏、选区遮罩、贴图窗口
│   ├── beautify-flyout/     原生面板：任务清单与剪贴板列表
│   ├── beautify-settings/   原生设置中心：页面表格、布局、绘制、窗口
│   └── beautify-widget/     原生小组件栏：Direct2D 绘制、分层窗口、命中测试
├── app/                     Tauri 宿主：窗口、托盘、热键、自启动、IPC
└── tools/make_icon.py       生成应用图标（纯标准库）
```

### 模块契约

每个功能 crate 实现 `beautify_core::Module`，方法都是 `&self`：

```rust
pub trait Module: Send + Sync {
    fn name(&self) -> &'static str;
    fn is_enabled(&self, config: &Config) -> bool;
    fn start(&self, ctx: ModuleContext) -> ModuleResult;
    fn apply(&self, config: &Config) -> ModuleResult;
    fn stop(&self) -> ModuleResult;
}
```

模块自己持有线程与原生资源，宿主只负责按顺序启动、推送配置、退出。`Registry`
对每个模块做 panic 隔离——任务栏出问题不应该把剪贴板历史一起带走。

### 事件驱动，而不是轮询

这是本项目性能数字的来源，值得单独说明：

- **任务栏**：`SetWinEventHook` 监听前台窗口与窗口位置变化，`EVENT_OBJECT_LOCATIONCHANGE`
  只置一个脏标记并挂一个 120ms 的一次性定时器，避免拖动窗口时每帧重算。3 秒的兜底
  定时器只用于比对缓存值，状态没变就什么都不做。
- **剪贴板**：`AddClipboardFormatListener` + `WM_CLIPBOARDUPDATE`，复制时才醒。
- **媒体会话**：注册 WinRT 的 `MediaPropertiesChanged` / `PlaybackInfoChanged` /
  `TimelinePropertiesChanged`，回调只 `SetEvent` 唤醒工作线程。等待超时会被重算成
  「下一句歌词的时间」，所以歌词是精确唤醒而不是 10Hz 轮询。
- **频谱**：WASAPI 的 `SetEventHandle` 驱动，阻塞在内核里，只有真的有音频时才做 FFT。

---

### 截图与贴图

按 `F1`（或托盘菜单的「截图」）后整块桌面会压暗，指针处有十字线和 8 倍放大镜——
放大镜带坐标与像素取色，边缘对齐到像素靠的就是它。拖出要截的区域，`Esc` 或右键取消。
松开鼠标后：

- 以标准 `CF_DIB` 写入剪贴板，可以直接粘进聊天窗口、画图或 Office；
- 打开了 `auto_pin` 的话，还会在**原来的位置**生成一张贴图。

贴图是一个置顶小窗：**右上角有 ✕ 可以点掉**，拖动移动、滚轮以指针为中心缩放（最近邻，
不糊）、方向键微调（按住 Shift 一次 10px）、`Ctrl+C` 复制原图，`Esc` / `Delete` /
右键 / 双击同样关闭。托盘菜单的「关闭全部贴图」可以一次收掉。

按 `F3`（或托盘菜单的「贴图：剪贴板图片」）可以把剪贴板里现有的图片直接贴到屏幕上，
再按一次收起——与 Snipaste 的 F1 / F3 一致。剪贴板列表里图片行的图钉按钮做的是同一件
事：贴上去之后按钮高亮，从贴图窗口或 `F3` 关掉之后高亮随之消失。

整个过程不写临时文件：像素从屏幕到剪贴板或贴图窗口都在内存里。遮罩用的是同一张抓屏的
预乘暗化副本，所以选区内外是同一份像素亮度不同，不会出现颜色偏移。

### 歌词来源

设置中心里可以直接选平台，都**不需要 API Key 或签名**：

| 来源 | 说明 |
|---|---|
| 关闭 | 只读本地 `.lrc`，完全不联网 |
| 网易云音乐 | 默认。中文曲库覆盖最好 |
| QQ 音乐 | 中文曲库，带翻译字段 |
| 酷狗音乐 | 先按关键词查，查不到时走 songsearch 的 hash 兜底 |
| LRCLIB | 国际曲库，无版权限制的开源库 |
| 自定义 | 填 `{title}` / `{artist}` / `{album}` 模板 |

**除「关闭」外都会把当前歌曲的名称与歌手发送给对应平台**——这是查询歌词的必要条件，
所以设置里写明了，并且 `关闭` 是一等选项。「测试当前来源」按钮会用正在播放的歌实际请求
一次并告诉你结果（哪个平台、多少行），排错不用猜。

**查到的歌词只留在内存里，不会写进磁盘**：换歌即弃，下次播放重新查一次。这个目录
`%LOCALAPPDATA%\WinBeautify\lyrics` 因此是只读的，里面每个 `.lrc` 都是你自己放进去的。
命名为 `歌手 - 歌名.lrc`，**优先于任何在线结果**——这是唯一你能手工校正的来源。

---

## 许可证

MIT
