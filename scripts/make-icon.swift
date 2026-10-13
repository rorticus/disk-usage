// Draws the app icon (a small sunburst) as a 1024x1024 PNG.
// Usage: swift make-icon.swift <output.png>

import AppKit

let size: CGFloat = 1024
let out = CommandLine.arguments.count > 1 ? CommandLine.arguments[1] : "icon.png"

let rep = NSBitmapImageRep(
    bitmapDataPlanes: nil, pixelsWide: Int(size), pixelsHigh: Int(size),
    bitsPerSample: 8, samplesPerPixel: 4, hasAlpha: true, isPlanar: false,
    colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0)!
NSGraphicsContext.saveGraphicsState()
NSGraphicsContext.current = NSGraphicsContext(bitmapImageRep: rep)
let ctx = NSGraphicsContext.current!.cgContext

// Rounded-square background in the macOS icon grid (824pt body, 100pt inset).
let body = CGRect(x: 100, y: 100, width: 824, height: 824)
ctx.addPath(CGPath(roundedRect: body, cornerWidth: 185, cornerHeight: 185, transform: nil))
ctx.setFillColor(NSColor(calibratedWhite: 0.11, alpha: 1).cgColor)
ctx.fillPath()

let center = CGPoint(x: size / 2, y: size / 2)
let gap: CGFloat = 0.018

func sector(_ r0: CGFloat, _ r1: CGFloat, _ a0: CGFloat, _ a1: CGFloat, _ color: NSColor) {
    // Angles are fractions of a turn, clockwise from 12 o'clock.
    let s = .pi / 2 - (a0 + gap / 2) * 2 * .pi
    let e = .pi / 2 - (a1 - gap / 2) * 2 * .pi
    let p = CGMutablePath()
    p.addArc(center: center, radius: r1, startAngle: s, endAngle: e, clockwise: true)
    p.addArc(center: center, radius: r0, startAngle: e, endAngle: s, clockwise: false)
    p.closeSubpath()
    ctx.addPath(p)
    ctx.setFillColor(color.cgColor)
    ctx.fillPath()
}

func color(_ a0: CGFloat, _ a1: CGFloat, depth: Int) -> NSColor {
    let hue = ((a0 + a1) / 2).truncatingRemainder(dividingBy: 1)
    let d = CGFloat(depth)
    return NSColor(calibratedHue: hue, saturation: 0.78 - 0.12 * d, brightness: 0.82 + 0.06 * d, alpha: 1)
}

// Three rings: (start, end) fractions per level.
let rings: [[(CGFloat, CGFloat)]] = [
    [(0, 0.46), (0.46, 0.72), (0.72, 0.9), (0.9, 1.0)],
    [(0, 0.3), (0.3, 0.42), (0.46, 0.62), (0.62, 0.7), (0.72, 0.84), (0.9, 0.96)],
    [(0, 0.2), (0.2, 0.27), (0.3, 0.38), (0.46, 0.55), (0.72, 0.79)],
]
let radii: [CGFloat] = [130, 235, 320, 385]
for (depth, ring) in rings.enumerated() {
    for (a0, a1) in ring {
        sector(radii[depth] + 4, radii[depth + 1] - 4, a0, a1, color(a0, a1, depth: depth))
    }
}

NSGraphicsContext.restoreGraphicsState()
try! rep.representation(using: .png, properties: [:])!.write(to: URL(fileURLWithPath: out))
