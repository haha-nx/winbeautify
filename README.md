# WinBeautify

Windows 桌面美化与增强工具。任务栏材质、任务栏歌词与实时频谱、剪贴板历史、任务清单，
全部跑在一个 Rust 进程里，空闲时不做任何轮询。

参考了 TranslucentTB、Rainmeter、PowerToys、ModernFlyouts 的做法，用 Rust + Tauri 2 实现。

---

## 功能

| 模块 | 内容 |
|---|---|
| **任务栏** | 全透明 / 模糊 / 亚克力 Acrylic / 云母 Mica / 纯色，自定义着色与不透明度，副屏任务栏，动态模式（窗口最大化时切换效果），全屏应用自动暂停，退出时还原 |
| **小组件栏** | 嵌入任务栏的 Widget Bar：启动器 + 自适应宽度的音频组件 |
| **任务栏歌词** | 通过 GSMTC 读取当前曲目，LRC 解析、可选在线接口（结果只存内存），歌词偏移补偿 |
| **音乐频谱** | WASAPI 回环捕获系统音频 + rustfft，对数分频，无需虚拟声卡 |
| **媒体控制** | 指针悬停时显示上一首 / 播放暂停 / 下一首 |
| **剪贴板** | 事件驱动监听，文本 / 图片 / 文件列表，搜索、收藏、去重、容量上限 |
| **任务清单** | SQLite 存储，Flyout 内增删改查，Markdown / JSON 导出 |
| **截图贴图** | 拖动选区截屏，直接写入剪贴板（CF_DIB），可把结果贴回屏幕当参考图，全程不落临时文件 |
| **设置中心** | 全部配置的图形界面，改动即时生效并写入 `config.toml`；原生 Direct2D 窗口，不再依赖 WebView2 |

---

## 快速开始

```bash
# 前端
cd ui
npm install
npm run build

# 后端
cd ..
cargo build --release -p winbeautify
./target/release/winbeautify.exe
```

启动后常驻托盘，左键点托盘图标打开设置，右键菜单可以打开 Flyout、重载配置或退出。

### 测试

```bash
cargo test --workspace                  # 纯逻辑测试，不需要桌面
cargo test -p beautify-settings -- --ignored   # 真的开一个设置窗口，开→关→再开
cargo clippy --workspace --all-targets
```

默认跑的是不依赖桌面的那些：布局、命中测试、像素运算、配置读写。真正需要窗口的测试
标记了 `#[ignore]`，上面第二条是其中一个——它验证设置窗口能开、能关、还能再开一次。
这条不是形式：`DestroyWindow` 会在处理消息的过程中同步投递 `WM_NCDESTROY`，重入消息
过程曾让关闭设置窗口直接 abort 整个进程，这个测试就是那次缺陷的回归守卫。

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
│   ├── beautify-settings/   原生设置中心：页面表格、布局、绘制、窗口
│   └── beautify-widget/     原生小组件栏：Direct2D 绘制、分层窗口、命中测试
├── app/                     Tauri 宿主：窗口、托盘、热键、自启动、IPC
├── ui/                      两个页面的前端（vanilla TS + Vite）
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

## 配置

配置文件在 `%LOCALAPPDATA%\WinBeautify\config.toml`，设置中心里改的每一项都会写回这里，
手改文件后用托盘菜单的「重新载入配置」即可生效。数据库与歌词目录在同目录。

几个值得注意的选项：

```toml
[taskbar]
mode = "acrylic"            # normal | clear | blur | acrylic | mica | opaque
dynamic_mode = true         # 窗口最大化时切换到 dynamic_mode_override
hide_on_fullscreen = true   # 全屏应用前台时还原为系统默认
restore_on_exit = true      # 退出时把任务栏还给 Windows

[media]
demo_mode = false           # 预览模式：伪造曲目/歌词/频谱，用来看外观
lyric_provider = "netease"  # off | netease | qq | kugou | lrclib | custom
online_api = ""             # 仅 lyric_provider = "custom" 时使用

[widget]
anchor = "taskbar-right"    # taskbar-* / bottom-*
lyric_min_width = 96        # 宽度跟随歌词，这两项是下限/上限（物理像素）
lyric_max_width = 280
audio_min_width = 168       # 整个音频组件的下限/上限
audio_max_width = 420

[snip]
enabled = true
hotkey = "Ctrl+Alt+A"       # 留空则只留托盘入口
copy_to_clipboard = true    # 以标准 CF_DIB 写入剪贴板
auto_pin = false            # 截完是否顺手贴回屏幕
dim = 0.45                  # 选区外的遮罩深度
```

### 截图与贴图

按 `Ctrl+Alt+A`（或托盘菜单的「截图」）后整块桌面会压暗，拖出要截的区域即可；`Esc`
或右键取消。松开鼠标后：

- 以标准 `CF_DIB` 写入剪贴板，可以直接粘进聊天窗口、画图或 Office；
- 打开了 `auto_pin` 的话，还会在**原来的位置**生成一张贴图。

贴图是一个置顶小窗：拖动移动、滚轮以指针为中心缩放（最近邻，不糊）、方向键微调（按住
Shift 一次 10px）、`Ctrl+C` 复制原图、`Esc` / `Delete` / 右键 / 双击关闭。托盘菜单的
「关闭全部贴图」可以一次收掉。

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

## 实测资源占用

在 2560×1440 @125% 的 Windows 11 上测得，release 构建，媒体模块开启：

| 配置 | 进程数 | 内存 | CPU |
|---|---|---|---|
| 小组件栏用 WebView2（旧实现） | 7 | 351 MB | 空闲 0.00% |
| 小组件栏用原生渲染（当前默认） | **1** | **约 39 MB** | 空闲约 0.2%；频谱播放时约 2.1% |
| 不显示小组件栏 | 1 | 15 MB | 0.00% |
| 原生渲染 + 打开着设置窗口 | 1 | 79 MB | — |
| 原生渲染 + 首次打开过 Flyout | 7 | 约 230–290 MB | — |

两点需要如实说明：

- **设置中心已经是原生窗口**，画在宿主进程里：同一进程实测**开着 79 MB、关掉 62 MB**，
  也就是约 17 MB，不新增进程。改成原生之前它是一整套 Chromium 进程（实测约 190 MB）。
  窗口关闭即销毁，下次打开重建。
- **Flyout 仍然是 WebView2**：按需创建，但关闭后只隐藏（重建一个 webview 要几百毫秒，
  点击时很卡），所以**第一次打开 Flyout 之后，Chromium 进程树会一直留到退出**。

也就是说 39 MB 是「只用小组件栏」的数字；一旦用过 Flyout，进程数就回到 7 个。下一步
把 Flyout 也改成原生绘制，才能让常驻占用稳定在几十 MB——它是一个列表面板（勾选、行内
编辑、搜索、滚动、图片缩略图），工作量明显大于一条胶囊，所以这一轮没有做。

**小组件栏改用 Direct2D 原生绘制，内存从 351 MB 降到 39 MB（约 −89%）。**
下面这段说明了为什么之前做不到，以及为什么现在可以。

小组件栏原本是一个 Tauri WebView2 窗口。WebView2 无论内容多简单，都会拉起一整套
Chromium 进程（主进程、GPU、渲染器、工具进程……），代价在 300 MB 量级。任何 WebView
方案的宿命都是如此——Tauri 省的是 Electron 的 Node 运行时，省不掉 Chromium 本身。

现在的实现完全不引入浏览器：用 `WS_EX_LAYERED` + `UpdateLayeredWindow` 拿到逐像素
alpha，用 Direct2D + DirectWrite + WIC 直接画那一个圆角胶囊（启动器、封面、歌词、
频谱）。全部在宿主进程内，没有额外的进程、没有额外的运行时。

### 为什么用分层窗口而不是 DirectComposition

分层窗口是最老、最可靠的逐像素透明方案，而且不依赖 GPU 合成器——本项目所在的虚拟机
恰好就是「DWM/DirectComposition 路径不工作」的那类环境（WebView2 的透明窗口在这里
根本不显示内容）。`UpdateLayeredWindow` 在这台机器上 0% / 25% / 50% / 100% 四档
alpha 全部正确合成。

它还带来一个额外好处：**分层窗口按 alpha 做命中测试**，所以胶囊周围全透明的部分会把
点击透传给任务栏。因此窗口可以一次性按最大宽度创建、之后再也不 resize，只让内部胶囊
做宽度动画——既省掉了窗口尺寸与动画不同步的所有竞态，也不会挡住任务栏的点击。

代价是失去 DWM 的亚克力模糊背景（分层窗口不能叠加系统 backdrop）。胶囊是一块半透明
纯色，而不是磨砂玻璃。设置里的背景不透明度因此默认调到 0.78：没有模糊层托底时，
太透会让文字对比度不足。

### 仍然想用 WebView2

设置中心 →「小组件栏」→「渲染方式」可以切回 WebView2。两条路径功能一致，保留它一是
作为原生渲染出问题时的回退，二是方便直接对比。切换即时生效。

---

## 已知限制

**Windows 11 22H2 之后的任务栏完全不理会材质设置。**
`SetWindowCompositionAttribute`（TranslucentTB 等工具在 Windows 10 时代依赖的未公开 API）
在这些 build 上**仍然返回成功，然后什么都不做**——不报错，也没有任何视觉变化。
25H2（build 26200）上实测：把背景模式设成「纯色」并指定纯红，任务栏颜色纹丝不动。
原因是任务栏自 22H2 起改由 `explorer.exe` 里的 XAML 绘制，背景是一个 XAML 矩形而不是窗口
表面，所以没有窗口表面可供染色。

设置中心会如实显示「系统自带任务栏不响应（Win11 22H2 起）」，而不是谎报「已应用」；
启动日志里也有一条对应的 WARN。想要真正生效，只能像 TranslucentTB 那样往 `explorer.exe`
里注入 DLL、通过 XAML 诊断通道改写 `Taskbar.TaskbarFrame` 下 `BackgroundFill` 的 `Fill`——
那是另一套独立组件，目前没有实现。系统设置里的「透明效果」不受影响，仍然有效。

Windows 10 与 21H2 及更早的 Windows 11 不受此限制。

**小组件栏是任务栏的附属窗口。**
为了稳定地浮在任务栏之上，它通过 `GWLP_HWNDPARENT` 认 `Shell_TrayWnd` 做 owner；
Explorer 重启后会重新认领。这不会出现在 Alt+Tab 或任务栏里。

**托盘图标是唯一可靠的入口。**
全局热键依赖 `RegisterHotKey`，与其它软件冲突时会注册失败（日志中有记录）；启动器按钮
需要点击事件能到达 WebView2，在远程桌面或非交互式会话中不一定成立。

**原生窗口没有无障碍树。**
小组件栏和设置中心都是一块 Direct2D 画布，屏幕阅读器看不到其中的按钮、下拉和文本——
它们之前的 WebView2 版本是有无障碍树的。小组件栏可以切回 WebView2 渲染；设置中心没有
这条退路，它已经把 WebView2 版本删掉了。这是用内存换来的代价，需要无障碍支持的话目前
只能用键盘热键把功能调出来的那部分（截图、Flyout 导航）。

**设置中心的文本输入不是系统控件。**
三个文本框（两个热键、一个自定义歌词接口地址）是就地绘制的，不是子 `EDIT` 控件——子窗口
无法合成进 Direct2D 表面。因此它们**不支持输入法**，只有 ASCII 能正常输入；正在编辑时
会画一条插入符，`Ctrl+V` / `Ctrl+C` 可用。这几项本来就只存加速键和 URL。

**在线歌词接口需要自己配置。**
默认不发任何网络请求。填了 `online_api` 之后走 WinHTTP，支持
`{title}` / `{artist}` / `{album}` 占位符，也能解包一层 JSON 的 `lyric` / `lrc` 字段。

---

## 本机构建环境说明

这台机器上 `cargo` 偶尔会挑到 Git Bash 自带的 GNU `link.exe`，另外 MSVC 的 `cl.exe`
需要显式设置 `INCLUDE`。构建前先设置：

```bash
export PATH="/d/Program Files/Microsoft Visual Studio/18/Community/VC/Tools/MSVC/14.29.30133/bin/Hostx64/x64:$PATH"
export LIB='D:\Windows Kits\10\Lib\10.0.26100.0\um\x64;D:\Windows Kits\10\Lib\10.0.26100.0\ucrt\x64;D:\Program Files\Microsoft Visual Studio\18\Community\VC\Tools\MSVC\14.29.30133\lib\x64'
export INCLUDE='D:\Windows Kits\10\Include\10.0.26100.0\ucrt;D:\Windows Kits\10\Include\10.0.26100.0\um;D:\Windows Kits\10\Include\10.0.26100.0\shared;D:\Program Files\Microsoft Visual Studio\18\Community\VC\Tools\MSVC\14.29.30133\include'
```

`libsqlite3-sys` 用 `bundled` 编译 SQLite，因此需要可用的 C 编译器。

---

## 许可证

MIT
