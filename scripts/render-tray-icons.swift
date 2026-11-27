// Render the menu bar icons from their SVG sources (macOS).
// Usage, from the repository root: swift scripts/render-tray-icons.swift
// Writes 44 px (22 pt at 2x) PNGs with transparent backgrounds.
import AppKit

let renders = [
    ("src-tauri/icons/tray-on.svg", "src-tauri/icons/tray-on.png"),
    ("src-tauri/icons/tray-off.svg", "src-tauri/icons/tray-off.png"),
    // the Settings page draws its menu bar preview with the same bell
    ("src-tauri/icons/tray-on.svg", "src/tray-glyph.png"),
]
let pixels = 44
let points = NSSize(width: pixels / 2, height: pixels / 2)

for (source, output) in renders {
    guard let image = NSImage(contentsOf: URL(fileURLWithPath: source)) else {
        fputs("cannot read \(source)\n", stderr)
        exit(1)
    }
    guard let bitmap = NSBitmapImageRep(
        bitmapDataPlanes: nil, pixelsWide: pixels, pixelsHigh: pixels, bitsPerSample: 8,
        samplesPerPixel: 4, hasAlpha: true, isPlanar: false, colorSpaceName: .deviceRGB,
        bytesPerRow: 0, bitsPerPixel: 0)
    else {
        fputs("cannot make a bitmap for \(output)\n", stderr)
        exit(1)
    }
    bitmap.size = points
    NSGraphicsContext.saveGraphicsState()
    NSGraphicsContext.current = NSGraphicsContext(bitmapImageRep: bitmap)
    image.draw(in: NSRect(origin: .zero, size: points))
    NSGraphicsContext.restoreGraphicsState()
    guard let png = bitmap.representation(using: .png, properties: [:]) else {
        fputs("cannot encode \(output)\n", stderr)
        exit(1)
    }
    do {
        try png.write(to: URL(fileURLWithPath: output))
    } catch {
        fputs("cannot write \(output): \(error)\n", stderr)
        exit(1)
    }
    print("wrote \(output)")
}
