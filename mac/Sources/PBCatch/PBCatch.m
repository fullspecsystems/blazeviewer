#import "include/PBCatch.h"

NSError *PBCatchException(void (NS_NOESCAPE ^block)(void)) {
    @try {
        block();
        return nil;
    } @catch (NSException *e) {
        NSString *reason = e.reason ?: e.name;
        return [NSError errorWithDomain:@"PBObjCException"
                                   code:-1
                               userInfo:@{NSLocalizedDescriptionKey : reason}];
    }
}
