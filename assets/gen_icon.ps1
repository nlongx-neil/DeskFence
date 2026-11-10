Add-Type -AssemblyName System.Drawing

# 用法：powershell -NoProfile -ExecutionPolicy Bypass -File assets/gen_icon.ps1
# 输出：与本脚本同目录的 deskkeeper.ico
# 设计：圆角方块（蓝靛渐变）+ 2x2 圆角模块（三块浅色整理格 + 一块琥珀强调格代表剪贴板/收集）
# 所有档位先在 1024x1024 以矢量方式绘制，再逐级高质量降采样，保证 16/24 仍清晰。
# 脚本可重复执行（幂等），每次覆盖写 assets/deskkeeper.ico。

$out = $PSScriptRoot
$sizes = @(16, 20, 24, 32, 40, 48, 64, 96, 128, 256)
$canvasSize = 1024

# ---------- 基础工具 ----------
function New-RoundedRect {
    param([double]$X, [double]$Y, [double]$W, [double]$H, [double]$R)
    $path = New-Object System.Drawing.Drawing2D.GraphicsPath
    $d = [Math]::Min($R * 2, [Math]::Min($W, $H))
    $path.AddArc([single]$X, [single]$Y, [single]$d, [single]$d, 180, 90)
    $path.AddArc([single]($X + $W - $d), [single]$Y, [single]$d, [single]$d, 270, 90)
    $path.AddArc([single]($X + $W - $d), [single]($Y + $H - $d), [single]$d, [single]$d, 0, 90)
    $path.AddArc([single]$X, [single]($Y + $H - $d), [single]$d, [single]$d, 90, 90)
    $path.CloseFigure()
    return $path
}

function New-FillBrush {
    param([System.Drawing.RectangleF]$Rect, [System.Drawing.Color[]]$Colors, [float[]]$Positions)
    $brush = [System.Drawing.Drawing2D.LinearGradientBrush]::new(
        $Rect,
        $Colors[0],
        $Colors[$Colors.Count - 1],
        [System.Drawing.Drawing2D.LinearGradientMode]::Vertical)
    $blend = [System.Drawing.Drawing2D.ColorBlend]::new($Colors.Count)
    $blend.Colors = $Colors
    $blend.Positions = $Positions
    $brush.InterpolationColors = $blend
    return $brush
}

function Resize-HighQuality {
    param([System.Drawing.Bitmap]$Source, [int]$Target)
    $current = $Source
    $w = $Source.Width
    # 逐级折半到 1.5 倍以内，最后一步 bicubic 收缩，避免一次性大幅缩小产生的糊边
    while ($w -gt ($Target * 1.5)) {
        $w = [int][Math]::Floor($w / 2)
        $step = [System.Drawing.Bitmap]::new($w, $w, [System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
        $sg = [System.Drawing.Graphics]::FromImage($step)
        $sg.CompositingMode = [System.Drawing.Drawing2D.CompositingMode]::SourceCopy
        $sg.CompositingQuality = [System.Drawing.Drawing2D.CompositingQuality]::HighQuality
        $sg.InterpolationMode = [System.Drawing.Drawing2D.InterpolationMode]::HighQualityBicubic
        $sg.PixelOffsetMode = [System.Drawing.Drawing2D.PixelOffsetMode]::HighQuality
        $sg.SmoothingMode = [System.Drawing.Drawing2D.SmoothingMode]::AntiAlias
        $sg.DrawImage($current, ([System.Drawing.Rectangle]::new(0, 0, $w, $w)))
        $sg.Dispose()
        if ($current -ne $Source) { $current.Dispose() }
        $current = $step
    }

    $result = [System.Drawing.Bitmap]::new($Target, $Target, [System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
    $rg = [System.Drawing.Graphics]::FromImage($result)
    $rg.CompositingMode = [System.Drawing.Drawing2D.CompositingMode]::SourceCopy
    $rg.CompositingQuality = [System.Drawing.Drawing2D.CompositingQuality]::HighQuality
    $rg.InterpolationMode = [System.Drawing.Drawing2D.InterpolationMode]::HighQualityBicubic
    $rg.PixelOffsetMode = [System.Drawing.Drawing2D.PixelOffsetMode]::HighQuality
    $rg.SmoothingMode = [System.Drawing.Drawing2D.SmoothingMode]::AntiAlias
    $rg.DrawImage($current, ([System.Drawing.Rectangle]::new(0, 0, $Target, $Target)))
    $rg.Dispose()
    if ($current -ne $Source) { $current.Dispose() }
    return $result
}

# ---------- 1024 主视觉 ----------
$bmp = [System.Drawing.Bitmap]::new($canvasSize, $canvasSize, [System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
$g = [System.Drawing.Graphics]::FromImage($bmp)
$g.SmoothingMode = [System.Drawing.Drawing2D.SmoothingMode]::AntiAlias
$g.CompositingQuality = [System.Drawing.Drawing2D.CompositingQuality]::HighQuality
$g.PixelOffsetMode = [System.Drawing.Drawing2D.PixelOffsetMode]::HighQuality
$g.InterpolationMode = [System.Drawing.Drawing2D.InterpolationMode]::HighQualityBicubic
$g.Clear([System.Drawing.Color]::Transparent)

# 圆角方块几何：四周留 6% 透明边距，圆角半径约为方块边长 22%
$tileX = 60.0; $tileY = 56.0; $tileW = 904.0; $tileH = 904.0; $tileR = 200.0
$tileRect = [System.Drawing.RectangleF]::new([single]$tileX, [single]$tileY, [single]$tileW, [single]$tileH)
$tilePath = New-RoundedRect $tileX $tileY $tileW $tileH $tileR

# 1) 柔和外投影（浅色背景上有落地感）
for ($i = 13; $i -ge 0; $i--) {
    $e = 4.0 + $i * 5.0
    $p = New-RoundedRect ($tileX - $e) ($tileY + 22 - $e) ($tileW + 2 * $e) ($tileH + 2 * $e) ($tileR + $e)
    $b = [System.Drawing.SolidBrush]::new([System.Drawing.Color]::FromArgb(4, 8, 18, 58))
    $g.FillPath($b, $p)
    $b.Dispose(); $p.Dispose()
}

# 2) 外发光（浅蓝，深色任务栏上勾出边缘）
for ($i = 9; $i -ge 0; $i--) {
    $e = 3.0 + $i * 2.6
    $p = New-RoundedRect ($tileX - $e) ($tileY - $e) ($tileW + 2 * $e) ($tileH + 2 * $e) ($tileR + $e)
    $b = [System.Drawing.SolidBrush]::new([System.Drawing.Color]::FromArgb(3, 158, 194, 255))
    $g.FillPath($b, $p)
    $b.Dispose(); $p.Dispose()
}

# 3) 主体：蓝靛对角渐变（双色平滑过渡，避免多段渐变出现色带）
$tileBrush = [System.Drawing.Drawing2D.LinearGradientBrush]::new(
    $tileRect,
    [System.Drawing.Color]::FromArgb(255, 160, 197, 255),
    [System.Drawing.Color]::FromArgb(255, 55, 69, 198),
    [System.Drawing.Drawing2D.LinearGradientMode]::ForwardDiagonal)
$g.FillPath($tileBrush, $tilePath)
$tileBrush.Dispose()

# 4) 顶部柔光（原生图标的玻璃质感，不产生细线）
$g.SetClip($tilePath, [System.Drawing.Drawing2D.CombineMode]::Replace)
$glossBrush = New-FillBrush $tileRect @(
    [System.Drawing.Color]::FromArgb(66, 255, 255, 255),
    [System.Drawing.Color]::FromArgb(18, 255, 255, 255),
    [System.Drawing.Color]::FromArgb(0, 255, 255, 255),
    [System.Drawing.Color]::FromArgb(0, 255, 255, 255)
) @(0.0, 0.32, 0.66, 1.0)
$g.FillRectangle($glossBrush, $tileRect)
$glossBrush.Dispose()
# 底部内阴影，增加厚度
$bottomBrush = New-FillBrush $tileRect @(
    [System.Drawing.Color]::FromArgb(0, 18, 28, 96),
    [System.Drawing.Color]::FromArgb(0, 18, 28, 96),
    [System.Drawing.Color]::FromArgb(34, 18, 28, 96)
) @(0.0, 0.45, 1.0)
$g.FillRectangle($bottomBrush, $tileRect)
$bottomBrush.Dispose()
$g.ResetClip()

# 5) 2x2 圆角模块：3 个浅色整理格 + 1 个琥珀强调格（剪贴板/收集）
#    网格 640，模块 280，间距 80：16px 下模块约 4.4px、间隙约 1.25px，不会出现亚像素细节
$cell = 280.0
$gap = 80.0
$grid = $cell * 2 + $gap
$gridX = (1024 - $grid) / 2
$gridY = (1024 - $grid) / 2
$cellR = 62.0

$lightCells = @(
    @($gridX, $gridY),
    @(($gridX + $cell + $gap), $gridY),
    @($gridX, ($gridY + $cell + $gap))
)
foreach ($c in $lightCells) {
    $cp = New-RoundedRect $c[0] $c[1] $cell $cell $cellR
    $cb = New-FillBrush ([System.Drawing.RectangleF]::new([single]$c[0], [single]$c[1], [single]$cell, [single]$cell)) @(
        [System.Drawing.Color]::FromArgb(240, 250, 253, 255),
        [System.Drawing.Color]::FromArgb(226, 219, 232, 255)
    ) @(0.0, 1.0)
    $g.FillPath($cb, $cp)
    $cb.Dispose(); $cp.Dispose()
}

# 强调格（右下）：琥珀渐变 + 内部浅色「卡片」，语义为收集/剪贴板
$ax = $gridX + $cell + $gap
$ay = $gridY + $cell + $gap
$ap = New-RoundedRect $ax $ay $cell $cell $cellR
$ab = New-FillBrush ([System.Drawing.RectangleF]::new([single]$ax, [single]$ay, [single]$cell, [single]$cell)) @(
    [System.Drawing.Color]::FromArgb(255, 255, 206, 104),
    [System.Drawing.Color]::FromArgb(255, 255, 162, 62)
) @(0.0, 1.0)
$g.FillPath($ab, $ap)
$ab.Dispose(); $ap.Dispose()

$cardInset = 73.0
$cardX = $ax + $cardInset
$cardY = $ay + $cardInset + 6.0
$cardW = $cell - $cardInset * 2
$cardH = $cell - $cardInset * 2 - 12.0
$cardPath = New-RoundedRect $cardX $cardY $cardW $cardH 36.0
$cardBrush = New-FillBrush ([System.Drawing.RectangleF]::new([single]$cardX, [single]$cardY, [single]$cardW, [single]$cardH)) @(
    [System.Drawing.Color]::FromArgb(240, 255, 255, 255),
    [System.Drawing.Color]::FromArgb(214, 255, 246, 226)
) @(0.0, 1.0)
$g.FillPath($cardBrush, $cardPath)
$cardBrush.Dispose(); $cardPath.Dispose()

# ---------- 降采样并打包 ICO ----------
$frames = @()
foreach ($s in $sizes) {
    $frame = Resize-HighQuality $bmp $s
    $ms = New-Object System.IO.MemoryStream
    $frame.Save($ms, [System.Drawing.Imaging.ImageFormat]::Png)
    $frames += , $ms.ToArray()
    $ms.Dispose()
    $frame.Dispose()
}
$g.Dispose(); $bmp.Dispose()

$headerSize = 6 + 16 * $sizes.Count
$offset = $headerSize
$entries = @()
for ($i = 0; $i -lt $sizes.Count; $i++) {
    $s = $sizes[$i]
    $w = if ($s -ge 256) { 0 } else { $s }   # 256 在 ICO 目录项中用 0 表示
    $data = $frames[$i]
    $entries += [pscustomobject]@{ W = $w; H = $w; Bytes = $data.Length; Offset = $offset }
    $offset += $data.Length
}

$ms2 = New-Object System.IO.MemoryStream
$bw = [System.IO.BinaryWriter]::new($ms2)
$bw.Write([uint16]0)                    # reserved
$bw.Write([uint16]1)                    # type: icon
$bw.Write([uint16]$sizes.Count)         # frame count
for ($i = 0; $i -lt $entries.Count; $i++) {
    $e = $entries[$i]
    $bw.Write([byte]$e.W)
    $bw.Write([byte]$e.H)
    $bw.Write([byte]0)                  # 调色板颜色数（0 = 无）
    $bw.Write([byte]0)                  # reserved
    $bw.Write([uint16]1)                # planes
    $bw.Write([uint16]32)               # bits per pixel
    $bw.Write([uint32]$e.Bytes)
    $bw.Write([uint32]$e.Offset)
}
foreach ($f in $frames) { $bw.Write($f) }
$bw.Flush()
$icoPath = Join-Path $out 'deskkeeper.ico'
[System.IO.File]::WriteAllBytes($icoPath, $ms2.ToArray())

"ico path=$icoPath"
"ico bytes=$((Get-Item $icoPath).Length) frames=$($sizes.Count) sizes=$($sizes -join ',')"
