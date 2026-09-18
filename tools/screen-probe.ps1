# 屏幕取样探针 —— 用来客观验证"画面真的变了"
#
# 为什么需要它：任务栏默认就是半透明的，靠肉眼很容易把"什么都没发生"看成生效；
# 而日志里的 `appearance applied` 只说明 `set_fill` 返回了 S_OK，不代表像素变了。
#
# 用法（PowerShell 7）：
#   pwsh -NoProfile -File tools\screen-probe.ps1                # 默认取点
#   pwsh -NoProfile -File tools\screen-probe.ps1 -Row 2120      # 整行扫一遍找细线
#   pwsh -NoProfile -File tools\screen-probe.ps1 -Points 600,2120,1200,2120
#
# 判读：
#   任务栏内取点 == 任务栏上方（壁纸）取点  → 真透明
#   任务栏内取点 == 配置的 #RRGGBB          → 不透明纯色
#   介于两者之间                            → 半透明 tint（按 alpha 混合）
#   细线开关：扫任务栏最上沿那一行，看有没有一条与相邻行不同的 1~2px 亮/暗线

param(
    # 任务栏内的取样点（物理像素，可写多组 x,y）
    [int[]]$Points = @(600, 2120, 1200, 2120, 2400, 2120, 600, 2050, 1200, 2050),
    # 给了 Row 就扫这一整行，用于找细线（每 200px 取一点）
    [int]$Row = 0
)

Add-Type -AssemblyName System.Drawing
$bmp = New-Object System.Drawing.Bitmap 1, 1
$g = [System.Drawing.Graphics]::FromImage($bmp)

function Sample([int]$x, [int]$y) {
    $g.CopyFromScreen($x, $y, 0, 0, (New-Object System.Drawing.Size 1, 1))
    $c = $bmp.GetPixel(0, 0)
    return "#{0:X2}{1:X2}{2:X2}" -f $c.R, $c.G, $c.B
}

if ($Row -gt 0) {
    Write-Output "row $Row:"
    for ($x = 200; $x -lt 3840; $x += 200) {
        Write-Output ("  x={0,-5} {1}" -f $x, (Sample $x $Row))
    }
} else {
    for ($i = 0; $i + 1 -lt $Points.Count; $i += 2) {
        $x = $Points[$i]; $y = $Points[$i + 1]
        Write-Output ("({0},{1}) = {2}" -f $x, $y, (Sample $x $y))
    }
}

$g.Dispose()
$bmp.Dispose()
