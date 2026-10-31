// Local lifecycle evidence only. No KLP admission or notification delivery.
#import <AppKit/AppKit.h>
#import <CoreGraphics/CoreGraphics.h>
#import <Security/Security.h>
#include <bsm/libbsm.h>
#include <libproc.h>
#include <sys/acl.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <unistd.h>
#include <fcntl.h>
#include <pwd.h>
#include <grp.h>

int kl_peer(int fd, audit_token_t *token) {
    socklen_t n = sizeof(*token);
    return getsockopt(fd, SOL_LOCAL, LOCAL_PEERTOKEN, token, &n) == 0 && n == sizeof(*token) ? 0 : -1;
}
int kl_self(audit_token_t *token) {
    mach_msg_type_number_t n = TASK_AUDIT_TOKEN_COUNT;
    return task_info(mach_task_self(), TASK_AUDIT_TOKEN, (task_info_t)token, &n) == KERN_SUCCESS && n == TASK_AUDIT_TOKEN_COUNT ? 0 : -1;
}
// Refuse extended ACLs, including grants invisible in the mode bits.
int kl_no_acl(int fd) {
    acl_t acl = acl_get_fd_np(fd, ACL_TYPE_EXTENDED);
    if (!acl) return errno == ENOENT ? 0 : -1;
    acl_entry_t entry;
    int result = acl_get_entry(acl, ACL_FIRST_ENTRY, &entry);
    int saved = errno;
    acl_free(acl);
    return result == -1 && saved == EINVAL ? 0 : -1;
}
int kl_image(audit_token_t *token, const char *path, uint64_t *device, uint64_t *inode) {
    @autoreleasepool {
        char running[PROC_PIDPATHINFO_MAXSIZE];
        if (token && (proc_pidpath_audittoken(token, running, sizeof(running)) <= 0 || strcmp(running, path))) return -1;
        // Pin the exact installed vnode. All ancestors must be root-owned,
        // non-symlink and non-writable by another principal.
        NSString *full = @(path);
        NSString *parent = [full stringByDeletingLastPathComponent];
        while (parent.length > 1) {
            struct stat st;
            if (lstat(parent.fileSystemRepresentation, &st) || !S_ISDIR(st.st_mode) || st.st_uid) return -1;
            bool systemApplications = [parent isEqualToString:@"/Applications"] && st.st_gid == 80 && (st.st_mode & 07777) == 0775;
            if ((st.st_mode & 0022) && !systemApplications) return -1;
            int dir = open(parent.fileSystemRepresentation, O_RDONLY | O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC);
            if (dir < 0) return -1;
            int safe = kl_no_acl(dir); close(dir);
            if (safe) return -1;
            parent = [parent stringByDeletingLastPathComponent];
        }
        int fd = open(path, O_RDONLY | O_NOFOLLOW | O_CLOEXEC | O_NONBLOCK);
        if (fd < 0) return -1;
        struct stat st;
        int valid = fstat(fd, &st) == 0 && S_ISREG(st.st_mode) && st.st_nlink == 1 && st.st_uid == 0 && st.st_gid == 0 && (st.st_mode & 07777) == 0755 && kl_no_acl(fd) == 0;
        close(fd);
        if (!valid) return -1;
        // Match the running image's CodeDirectory to the fixed installed app,
        // rather than assuming proc_pidpath refers to the current path's inode.
        NSData *audit = token ? [NSData dataWithBytes:token length:sizeof(*token)] : [NSData data];
        NSDictionary *attributes = @{(__bridge NSString *)kSecGuestAttributeAudit: audit};
        SecCodeRef guest = NULL;
        SecStaticCodeRef disk = NULL;
        CFDictionaryRef gi = NULL, di = NULL;
        OSStatus rc = token ? SecCodeCopyGuestWithAttributes(NULL, (__bridge CFDictionaryRef)attributes, kSecCSDefaultFlags, &guest) : 0;
        if (!rc) rc = SecStaticCodeCreateWithPath((__bridge CFURLRef)[NSURL fileURLWithPath:full], kSecCSDefaultFlags, &disk);
        if (!rc) rc = SecStaticCodeCheckValidity(disk, kSecCSStrictValidate, NULL);
        if (!rc && token) rc = SecCodeCopySigningInformation(guest, kSecCSSigningInformation, &gi);
        if (!rc) rc = SecCodeCopySigningInformation(disk, kSecCSSigningInformation, &di);
        bool same = !token && !rc;
        if (!rc && token) {
            CFTypeRef a = CFDictionaryGetValue(gi, kSecCodeInfoUnique), b = CFDictionaryGetValue(di, kSecCodeInfoUnique);
            same = a && b && CFEqual(a, b);
        }
        if (gi) CFRelease(gi); if (di) CFRelease(di); if (guest) CFRelease(guest); if (disk) CFRelease(disk);
        if (!same) return -1;
        // Recheck the vnode after Security's path based lookup.
        struct stat last;
        if (lstat(path, &last) || last.st_dev != st.st_dev || last.st_ino != st.st_ino || last.st_ctimespec.tv_sec != st.st_ctimespec.tv_sec || last.st_ctimespec.tv_nsec != st.st_ctimespec.tv_nsec) return -1;
        *device = (uint64_t)st.st_dev; *inode = st.st_ino;
        return 0;
    }
}
int kl_alive(audit_token_t *token) {
    char path[PROC_PIDPATHINFO_MAXSIZE];
    return proc_pidpath_audittoken(token, path, sizeof(path)) > 0;
}
int kl_signal(audit_token_t *token, int signal) {
    // The kernel checks PID generation atomically with signalling.
    return proc_signal_with_audittoken(token, signal);
}

@interface KLPresence : NSObject
@property BOOL unlocked;
@end
@implementation KLPresence
- (void)locked:(NSNotification *)note { (void)note; self.unlocked = NO; }
- (void)unlocked:(NSNotification *)note { (void)note; self.unlocked = YES; }
- (void)inactive:(NSNotification *)note { (void)note; self.unlocked = NO; }
@end
static KLPresence *presence;
void kl_presence_init(void) {
    @autoreleasepool {
        presence = [KLPresence new]; // Initially UNKNOWN, never infer unlocked.
        NSDistributedNotificationCenter *distributed = NSDistributedNotificationCenter.defaultCenter;
        [distributed addObserver:presence selector:@selector(locked:) name:@"com.apple.screenIsLocked" object:nil suspensionBehavior:NSNotificationSuspensionBehaviorDeliverImmediately];
        [distributed addObserver:presence selector:@selector(unlocked:) name:@"com.apple.screenIsUnlocked" object:nil suspensionBehavior:NSNotificationSuspensionBehaviorDeliverImmediately];
        NSNotificationCenter *workspace = NSWorkspace.sharedWorkspace.notificationCenter;
        [workspace addObserver:presence selector:@selector(inactive:) name:NSWorkspaceSessionDidResignActiveNotification object:nil];
        [workspace addObserver:presence selector:@selector(inactive:) name:NSWorkspaceWillSleepNotification object:nil];
    }
}
void kl_presence_invalidate(void) { presence.unlocked = NO; }
int kl_presence_poll(void) {
    @autoreleasepool {
        [NSRunLoop.currentRunLoop runUntilDate:[NSDate dateWithTimeIntervalSinceNow:0.01]];
        SecuritySessionId sid = 0;
        SessionAttributeBits attributes = 0;
        audit_token_t token;
        if (kl_self(&token) || SessionGetInfo(callerSecuritySession, &sid, &attributes)
            || sid != (SecuritySessionId)audit_token_to_asid(token) || !(attributes & sessionHasGraphicAccess)
            || (attributes & (sessionIsRoot | sessionIsRemote))) {
            presence.unlocked = NO; return 0;
        }
        CFDictionaryRef session = CGSessionCopyCurrentDictionary();
        if (!session) { presence.unlocked = NO; return 0; }
        NSDictionary *dict = CFBridgingRelease(session);
        bool active = [dict[(__bridge NSString *)kCGSessionUserIDKey] isEqual:@(geteuid())]
            && [dict[(__bridge NSString *)kCGSessionOnConsoleKey] isEqual:@YES]
            && [dict[(__bridge NSString *)kCGSessionLoginDoneKey] isEqual:@YES];
        // Poll the lock snapshot as an additional conservative check. Absence
        // of this undocumented key does not establish unlocked. Notifications
        // alone cannot authorize restore after a missed transition.
        NSNumber *locked = dict[@"CGSSessionScreenIsLocked"];
        if (!active || ![locked isKindOfClass:NSNumber.class] || locked.boolValue) { presence.unlocked = NO; return 0; }
        return presence.unlocked ? 1 : 0;
    }
}

int kl_user_context(void) {
    uid_t uid = geteuid();
    if (!uid || getuid() != uid || getgid() != getegid()) return -1;
    struct passwd record, *found = NULL;
    char buffer[65536];
    if (getpwuid_r(uid, &record, buffer, sizeof(buffer), &found) || !found || record.pw_gid != getegid()) return -1;
    int expected[256], actual[256], ne = 256;
    if (getgrouplist(record.pw_name, (int)record.pw_gid, expected, &ne) < 0 || ne > 256) return -1;
    int na = getgroups(256, (gid_t *)actual);
    if (na < 0) return -1;
    // Compare membership as sets (macOS can repeat the primary group).
    for (int i = 0; i < ne; i++) {
        bool present = expected[i] == (int)getegid();
        for (int j = 0; j < na; j++) present |= expected[i] == actual[j];
        if (!present) return -1;
    }
    for (int i = 0; i < na; i++) {
        bool present = actual[i] == (int)getegid();
        for (int j = 0; j < ne; j++) present |= actual[i] == expected[j];
        if (!present) return -1;
    }
    return 0;
}
