// ScreenCaptureKit bridge. All returned pixels are owned; no framework buffer
// crosses the FFI boundary. Called only inside the console user's child.
#import <Foundation/Foundation.h>
#import <ScreenCaptureKit/ScreenCaptureKit.h>
#import <CoreGraphics/CoreGraphics.h>
#import <SystemConfiguration/SystemConfiguration.h>
#import <Security/Security.h>
#import <CoreLocation/CoreLocation.h>

bool kanade_capture_supported(void) {
    if (@available(macOS 14.0, *)) return true;
    return false;
}

// Codes: 1 unsupported OS, 2 permission, 3 no display, 4 unavailable/error.
int kanade_capture(uint8_t **pixels, uint32_t *width, uint32_t *height,
                   char *reason, size_t reason_len) {
    @autoreleasepool {
        if (@available(macOS 14.0, *)) {
            if (!CGPreflightScreenCaptureAccess()) return 2;
            dispatch_semaphore_t ready = dispatch_semaphore_create(0);
            __block SCShareableContent *content = nil;
            __block NSError *failure = nil;
            [SCShareableContent getShareableContentExcludingDesktopWindows:NO
                onScreenWindowsOnly:YES completionHandler:^(SCShareableContent *c, NSError *e) {
                    content = c;
                    failure = e;
                    dispatch_semaphore_signal(ready);
                }];
            if (dispatch_semaphore_wait(ready, dispatch_time(DISPATCH_TIME_NOW, 3 * NSEC_PER_SEC))) {
                snprintf(reason, reason_len, "display enumeration timed out");
                return 4;
            }
            if (failure || !content) {
                snprintf(reason, reason_len, "%s", failure.localizedDescription.UTF8String ?: "display enumeration failed");
                return (failure.code == SCStreamErrorUserDeclined || !CGPreflightScreenCaptureAccess()) ? 2 : 4;
            }
            SCDisplay *display = nil;
            for (SCDisplay *candidate in content.displays) {
                if (candidate.displayID == CGMainDisplayID()) { display = candidate; break; }
            }
            if (!display) return 3;
            SCContentFilter *filter = [[SCContentFilter alloc] initWithDisplay:display
                excludingApplications:@[] exceptingWindows:@[]];
            SCStreamConfiguration *config = [SCStreamConfiguration new];
            config.width = display.width;
            config.height = display.height;
            config.showsCursor = YES;
            config.capturesAudio = NO;
            dispatch_semaphore_t captured = dispatch_semaphore_create(0);
            // ARC owns the retained image even if completion arrives after timeout.
            __block id ownedImage = nil;
            __block NSError *captureError = nil;
            [SCScreenshotManager captureImageWithFilter:filter configuration:config
                completionHandler:^(CGImageRef image, NSError *error) {
                    if (image) ownedImage = CFBridgingRelease(CGImageRetain(image));
                    captureError = error;
                    dispatch_semaphore_signal(captured);
                }];
            if (dispatch_semaphore_wait(captured, dispatch_time(DISPATCH_TIME_NOW, 3 * NSEC_PER_SEC))) {
                snprintf(reason, reason_len, "screenshot timed out");
                return 4;
            }
            if (captureError || !ownedImage) {
                snprintf(reason, reason_len, "%s", captureError.localizedDescription.UTF8String ?: "screenshot unavailable");
                return (captureError.code == SCStreamErrorUserDeclined || !CGPreflightScreenCaptureAccess()) ? 2 : 4;
            }
            CGImageRef image = (__bridge CGImageRef)ownedImage;
            size_t w = CGImageGetWidth(image), h = CGImageGetHeight(image);
            if (!w || !h || w > 16384 || h > 16384 || w * h > 64 * 1024 * 1024) {
                snprintf(reason, reason_len, "invalid or unsupported screen dimensions");
                return 4;
            }
            size_t stride = w * 4;
            uint8_t *buffer = calloc(h, stride);
            CGColorSpaceRef color = CGColorSpaceCreateWithName(kCGColorSpaceSRGB);
            CGContextRef ctx = CGBitmapContextCreate(buffer, w, h, 8, stride, color,
                kCGImageAlphaPremultipliedFirst | kCGBitmapByteOrder32Little);
            CGColorSpaceRelease(color);
            if (!buffer || !ctx) {
                free(buffer);
                if (ctx) CGContextRelease(ctx);
                snprintf(reason, reason_len, "cannot allocate screen pixels");
                return 4;
            }
            // CGBitmapContextDrawImage normalizes source format and row padding.
            CGContextDrawImage(ctx, CGRectMake(0, 0, w, h), image);
            CGContextRelease(ctx);
            *pixels = buffer;
            *width = (uint32_t)w;
            *height = (uint32_t)h;
            return 0;
        }
        return 1;
    }
}

// These entry points never enumerate displays or acquire pixels.
int64_t kanade_console_user(char *buffer, size_t length) {
    uid_t uid = 0;
    CFStringRef name = SCDynamicStoreCopyConsoleUser(NULL, &uid, NULL);
    if (!name) return -1;
    bool copied = CFStringGetCString(name, buffer, length, kCFStringEncodingUTF8);
    CFRelease(name);
    return copied ? (int64_t)uid : -1;
}

bool kanade_screen_permission_present(void) {
    return CGPreflightScreenCaptureAccess();
}

bool kanade_screen_permission_request(void) {
    return CGRequestScreenCaptureAccess();
}

// Stable signed identity, deliberately excluding version and cdhash. Unsigned /
// ad-hoc binaries fall back to the installed path in the durable history key.
bool kanade_signing_identity(char *buffer, size_t length) {
    @autoreleasepool {
        SecCodeRef code = NULL;
        CFDictionaryRef info = NULL;
        if (SecCodeCopySelf(kSecCSDefaultFlags, &code) != errSecSuccess) return false;
        OSStatus status = SecCodeCopySigningInformation(code, kSecCSSigningInformation, &info);
        CFRelease(code);
        if (status != errSecSuccess || !info) return false;
        NSDictionary *fields = CFBridgingRelease(info);
        NSString *team = fields[(__bridge NSString *)kSecCodeInfoTeamIdentifier];
        NSString *identifier = fields[(__bridge NSString *)kSecCodeInfoIdentifier];
        if (!team.length || !identifier.length) return false;
        NSString *identity = [NSString stringWithFormat:@"%@:%@", team, identifier];
        return [identity getCString:buffer maxLength:length encoding:NSUTF8StringEncoding];
    }
}

@interface KanadeLocationDelegate : NSObject <CLLocationManagerDelegate>
@property (nonatomic) BOOL finished;
@property (nonatomic, strong) CLLocation *location;
@property (nonatomic, strong) NSError *error;
@end

@implementation KanadeLocationDelegate
- (void)locationManager:(CLLocationManager *)manager didUpdateLocations:(NSArray<CLLocation *> *)locations {
    (void)manager;
    self.location = locations.lastObject;
    self.finished = YES;
}
- (void)locationManager:(CLLocationManager *)manager didFailWithError:(NSError *)error {
    (void)manager;
    self.error = error;
    self.finished = YES;
}
@end

// Status codes: 0 = Success, 1 = LocationDisabled, 2 = PermissionDenied, 3 = Unavailable, 4 = Timeout, 5 = NotDetermined
bool kanade_macos_acquire_location(int32_t *status_code, double *latitude, double *longitude, double *accuracy, int64_t *acquired_ms) {
    @autoreleasepool {
        if (![CLLocationManager locationServicesEnabled]) {
            *status_code = 1;
            return false;
        }

        CLLocationManager *tempMgr = [[CLLocationManager alloc] init];
        CLAuthorizationStatus status = tempMgr.authorizationStatus;

        if (status == kCLAuthorizationStatusDenied || status == kCLAuthorizationStatusRestricted) {
            *status_code = 2;
            return false;
        }

        if (status == kCLAuthorizationStatusNotDetermined) {
            *status_code = 5;
            return false;
        }
        KanadeLocationDelegate *delegate = [[KanadeLocationDelegate alloc] init];
        CLLocationManager *manager = [[CLLocationManager alloc] init];
        manager.delegate = delegate;
        manager.desiredAccuracy = kCLLocationAccuracyHundredMeters;

        // CLLocationManager delivers delegate callbacks on its creating thread's
        // RunLoop. This function runs on a blocking worker and pumps that loop.
        [manager requestLocation];
        double deadline = [NSProcessInfo processInfo].systemUptime + 5.0;
        while (!delegate.finished && [NSProcessInfo processInfo].systemUptime < deadline) {
            CFRunLoopRunInMode(kCFRunLoopDefaultMode, 0.05, true);
        }
        [manager stopUpdatingLocation];
        manager.delegate = nil;
        if (!delegate.finished) {
            *status_code = 4;
            return false;
        }

        if (delegate.location && delegate.location.horizontalAccuracy >= 0) {
            *latitude = delegate.location.coordinate.latitude;
            *longitude = delegate.location.coordinate.longitude;
            *accuracy = delegate.location.horizontalAccuracy;
            *acquired_ms = (int64_t)(delegate.location.timestamp.timeIntervalSince1970 * 1000.0);
            *status_code = 0;
            return true;
        }

        if (delegate.error) {
            if (delegate.error.code == kCLErrorDenied) {
                *status_code = 2;
            } else {
                *status_code = 3;
            }
            return false;
        }

        *status_code = 3;
        return false;
    }
}
