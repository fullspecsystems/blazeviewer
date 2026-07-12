import AVFoundation
import UniformTypeIdentifiers

/// Serves an in-RAM archive (ZIP/7z) video's bytes to `AVPlayer` through a custom-scheme
/// `AVURLAsset`, so an entry plays **without ever being written to disk** (privacy #2). The
/// whole container is resident (the archive layer inflated it into RAM), so every loading
/// request — content-info and arbitrary byte ranges from a seek — is answered synchronously
/// from the `Data`. `AVAssetResourceLoader` holds its delegate weakly, so `NativeVideoPlayer`
/// retains this for the session's lifetime.
final class ArchiveVideoLoader: NSObject, AVAssetResourceLoaderDelegate {
    /// The custom-scheme URL `AVPlayer` opens. A non-file, non-http scheme is exactly what
    /// routes every read through this delegate instead of the file/network loaders.
    let url: URL
    /// The delegate callbacks run here (off the main thread), reading the immutable `Data`.
    let queue = DispatchQueue(label: "ca.fullspec.photoblaze.archive-video-loader")

    private let data: Data
    private let contentType: String?  // UTI, e.g. "public.mpeg-4"

    init(data: Data, name: String) {
        self.data = data
        let ext = (name as NSString).pathExtension
        self.contentType = UTType(filenameExtension: ext)?.identifier
        var comps = URLComponents()
        comps.scheme = "pbarchive"
        comps.host = "entry"
        comps.path = "/" + (name.isEmpty ? "clip" : name)
        self.url = comps.url ?? URL(string: "pbarchive://entry/clip")!
        super.init()
    }

    func resourceLoader(
        _ resourceLoader: AVAssetResourceLoader,
        shouldWaitForLoadingOfRequestedResource loadingRequest: AVAssetResourceLoadingRequest
    ) -> Bool {
        // Content-information: AVPlayer needs the type + total length + range support up
        // front, or it won't issue byte-range reads (and stalls/fails).
        if let info = loadingRequest.contentInformationRequest {
            info.contentType = contentType
            info.contentLength = Int64(data.count)
            info.isByteRangeAccessSupported = true
        }
        // Data: serve the requested range straight from RAM. `currentOffset` advances as we
        // respond; everything is resident, so one respond + finish satisfies each request.
        if let dr = loadingRequest.dataRequest {
            let offset = Int(dr.currentOffset)
            if offset < data.count {
                let length =
                    dr.requestsAllDataToEndOfResource
                    ? data.count - offset
                    : min(dr.requestedLength, data.count - offset)
                dr.respond(with: data.subdata(in: offset..<(offset + length)))
            }
        }
        loadingRequest.finishLoading()
        return true
    }
}
