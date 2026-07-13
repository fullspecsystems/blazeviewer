#import <Foundation/Foundation.h>

NS_ASSUME_NONNULL_BEGIN

/// Run `block`, converting any Objective-C exception into an NSError (nil = no
/// exception). AVFoundation's audio engine throws NSExceptions — which Swift
/// cannot catch — for format/device failures; this shim turns "the audio graph
/// disliked something" into a recoverable nil-player instead of an abort()
/// (task #84 §7; the SessionAudioPlayer MKV crash).
NSError *_Nullable PBCatchException(void (NS_NOESCAPE ^block)(void));

NS_ASSUME_NONNULL_END
