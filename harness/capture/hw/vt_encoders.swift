// Research probe (not production): list every VideoToolbox encoder this device
// offers and try to create an MPEG-1 / MPEG-2 compression session directly.
import CoreMedia
import Foundation
import VideoToolbox

func fourcc(_ v: UInt32) -> String {
    String(bytes: [24, 16, 8, 0].map { UInt8((v >> $0) & 0xFF) }, encoding: .ascii) ?? "\(v)"
}

var list: CFArray?
VTCopyVideoEncoderList(nil, &list)
var encoders: [[String: Any]] = []
for e in (list as? [[String: Any]]) ?? [] {
    let type = (e[kVTVideoEncoderList_CodecType as String] as? NSNumber)?.uint32Value ?? 0
    encoders.append([
        "codec_type": fourcc(type),
        "codec_name": e[kVTVideoEncoderList_CodecName as String] ?? "",
        "encoder_name": e[kVTVideoEncoderList_EncoderName as String] ?? "",
        "encoder_id": e[kVTVideoEncoderList_EncoderID as String] ?? "",
        "hardware": e["IsHardwareAccelerated"] ?? NSNull(),
    ])
}
var attempts: [[String: Any]] = []
for (name, type) in [("mpeg1", kCMVideoCodecType_MPEG1Video), ("mpeg2", kCMVideoCodecType_MPEG2Video), ("h264-control", kCMVideoCodecType_H264)] {
    var session: VTCompressionSession?
    let status = VTCompressionSessionCreate(allocator: nil, width: 960, height: 540, codecType: type,
        encoderSpecification: nil, imageBufferAttributes: nil, compressedDataAllocator: nil,
        outputCallback: nil, refcon: nil, compressionSessionOut: &session)
    attempts.append(["codec": name, "fourcc": fourcc(type), "status": Int(status), "created": session != nil])
    if let s = session { VTCompressionSessionInvalidate(s) }
}
let out: [String: Any] = ["encoders": encoders, "session_attempts": attempts,
                          "os": ProcessInfo.processInfo.operatingSystemVersionString]
let data = try! JSONSerialization.data(withJSONObject: out, options: [.prettyPrinted, .sortedKeys])
print(String(data: data, encoding: .utf8)!)
