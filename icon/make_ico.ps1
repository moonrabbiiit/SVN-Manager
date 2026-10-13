# Build icon/repo-manager.ico from rabbit.png (16/24/32/48/64/128/256 px).
# build.rs feeds that .ico to windres, so it is what repo-manager.exe shows in
# Explorer, on the taskbar and in the title bar. To change the exe icon:
# replace rabbit.png, then run
#   powershell -NoProfile -ExecutionPolicy Bypass -File icon\make_ico.ps1
# Notes: keep this file ASCII-only - Windows PowerShell 5.1 reads BOM-less files
# as ANSI, so Chinese comments would break the parsing.
Add-Type -AssemblyName System.Drawing
$root = Split-Path -Parent $PSScriptRoot
$src = Join-Path $root "rabbit.png"
$out = Join-Path $PSScriptRoot "repo-manager.ico"
if (-not (Test-Path $src)) { "[!] source image not found: $src"; exit 1 }
$sizes = 16, 24, 32, 48, 64, 128, 256
$image = [Drawing.Image]::FromFile($src)
$entries = @()
foreach ($size in $sizes) {
    $bmp = New-Object Drawing.Bitmap($size, $size, [Drawing.Imaging.PixelFormat]::Format32bppArgb)
    $g = [Drawing.Graphics]::FromImage($bmp)
    $g.InterpolationMode = [System.Drawing.Drawing2D.InterpolationMode]::HighQualityBicubic
    $g.SmoothingMode = [Drawing.Drawing2D.SmoothingMode]::HighQuality
    $g.DrawImage($image, (New-Object Drawing.Rectangle(0, 0, $size, $size)))
    $g.Dispose()
    # ICO stores rows bottom-up, LockBits gives them top-down, so copy in reverse
    $data = New-Object 'byte[]' ($size * $size * 4)
    $mask = New-Object 'byte[]' (([Math]::Ceiling($size / 32.0) * 4) * $size)
    $rect = New-Object Drawing.Rectangle(0, 0, $size, $size)
    $item = $bmp.LockBits($rect, [Drawing.Imaging.ImageLockMode]::ReadOnly, [Drawing.Imaging.PixelFormat]::Format32bppArgb)
    $stride = $item.Stride
    $line = New-Object 'byte[]' ($stride)
    for ($row = 0; $row -lt $size; $row++) {
        [Runtime.InteropServices.Marshal]::Copy([IntPtr] ($item.Scan0.ToInt64() + ($row * $stride)), $line, 0, $stride)
        [Array]::Copy($line, 0, $data, (($size - 1 - $row) * $size * 4), ($size * 4))
    }
    $bmp.UnlockBits($item)
    $bmp.Dispose()
    $ms = New-Object IO.MemoryStream
    $w = New-Object IO.BinaryWriter($ms)
    # BITMAPINFOHEADER, biHeight = color rows + AND mask rows, 32bpp, BI_RGB
    $w.Write([UInt32]40); $w.Write([Int32]$size); $w.Write([Int32]($size * 2))
    $w.Write([UInt16]1); $w.Write([UInt16]32); $w.Write([UInt32]0)
    $w.Write([UInt32] ($data.Length + $mask.Length)); $w.Write([Int32]0); $w.Write([Int32]0)
    $w.Write([UInt32]0); $w.Write([UInt32]0)
    $w.Write($data); $w.Write($mask)
    $w.Flush()
    $entries += , @($size, $ms.ToArray())
    $ms.Dispose()
}
$image.Dispose()
$fs = New-Object IO.FileStream($out, [IO.FileMode]::Create)
$bw = New-Object IO.BinaryWriter($fs)
$bw.Write([UInt16]0); $bw.Write([UInt16]1); $bw.Write([UInt16] $entries.Count)
$offset = 6 + 16 * $entries.Count
foreach ($entry in $entries) {
    $edge = if ($entry[0] -ge 256) { 0 } else { $entry[0] }  # 256 px is written as 0
    $bw.Write([Byte]$edge); $bw.Write([Byte]$edge)
    $bw.Write([Byte]0); $bw.Write([Byte]0)
    $bw.Write([UInt16]1); $bw.Write([UInt16]32)
    $bw.Write([UInt32] $entry[1].Length); $bw.Write([UInt32]$offset)
    $offset += $entry[1].Length
}
foreach ($entry in $entries) { $bw.Write($entry[1]) }
$bw.Flush(); $fs.Close(); $bw.Dispose(); $fs.Dispose()
"[OK] $out ($((Get-Item $out).Length) bytes)"
foreach ($size in $sizes) { $icon = New-Object Drawing.Icon($out, $size, $size); "     readable $size -> $($icon.Width)x$($icon.Height)"; $icon.Dispose() }