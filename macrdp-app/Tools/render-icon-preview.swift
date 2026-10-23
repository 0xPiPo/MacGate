// Renders a built app's icon (as macOS draws it from AppIcon.icon) to a PNG.
// Usage: swift Tools/render-icon-preview.swift <path/to/macrdp.app> <out.png> [size]
// It only reads the bundle; it does not launch the app.
import AppKit

let args = CommandLine.arguments
guard args.count >= 3 else {
    FileHandle.standardError.write(Data("usage: render-icon-preview.swift <app> <out.png> [size]\n".utf8))
    exit(2)
}
let size = args.count > 3 ? Int(args[3]) ?? 512 : 512
let icon = NSWorkspace.shared.icon(forFile: URL(filePath: args[1]).standardizedFileURL.absoluteURL.path)

guard let rep = NSBitmapImageRep(bitmapDataPlanes: nil, pixelsWide: size, pixelsHigh: size,
                                 bitsPerSample: 8, samplesPerPixel: 4, hasAlpha: true, isPlanar: false,
                                 colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0) else { exit(1) }
NSGraphicsContext.saveGraphicsState()
NSGraphicsContext.current = NSGraphicsContext(bitmapImageRep: rep)
icon.draw(in: NSRect(x: 0, y: 0, width: size, height: size))
NSGraphicsContext.restoreGraphicsState()
try rep.representation(using: .png, properties: [:])!.write(to: URL(filePath: args[2]))
print("wrote \(args[2]) (\(size)px)")
