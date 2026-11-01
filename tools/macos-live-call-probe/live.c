
// Logs live AppKit / WindowServer / Accessibility / event entry points called by
// the platform-macos lib test binary (goat-si/cua-driver#188).
#include <ApplicationServices/ApplicationServices.h>
#include <objc/runtime.h>
#include <objc/message.h>
#include <dlfcn.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>
#include <mach-o/dyld.h>

#define INTERPOSE(rep, orig) \
  __attribute__((used)) static struct { const void *r; const void *o; } _ip_##orig \
  __attribute__((section("__DATA,__interpose"))) = { (const void *)(unsigned long)&rep, (const void *)(unsigned long)&orig };

static void live_log(const char *what, const char *detail) {
  char line[512];
  int n = snprintf(line, sizeof line, "LIVECALL %s %s\n", what, detail ? detail : "");
  write(2, line, n > 0 && n < (int)sizeof line ? n : (int)sizeof line - 1);
}

static const char *pid_kind(AXUIElementRef el) {
  pid_t pid = 0;
  if (!el || AXUIElementGetPid(el, &pid) != kAXErrorSuccess) return "pid=?";
  if (pid <= 0) return "pid<=0";
  if (pid == getpid()) return "pid=self";
  return kill(pid, 0) == 0 ? "pid=live" : "pid=dead";
}

#define AXWRAP(name, sig_args, call_args) \
  static AXError my_##name sig_args { live_log(#name, pid_kind(el)); return name call_args; } \
  INTERPOSE(my_##name, name)

AXWRAP(AXUIElementCopyAttributeValue, (AXUIElementRef el, CFStringRef a, CFTypeRef *v), (el, a, v))
AXWRAP(AXUIElementCopyMultipleAttributeValues, (AXUIElementRef el, CFArrayRef a, AXCopyMultipleAttributeOptions o, CFArrayRef *v), (el, a, o, v))
AXWRAP(AXUIElementSetAttributeValue, (AXUIElementRef el, CFStringRef a, CFTypeRef v), (el, a, v))
AXWRAP(AXUIElementPerformAction, (AXUIElementRef el, CFStringRef a), (el, a))
AXWRAP(AXUIElementCopyAttributeNames, (AXUIElementRef el, CFArrayRef *n), (el, n))
AXWRAP(AXUIElementCopyActionNames, (AXUIElementRef el, CFArrayRef *n), (el, n))
AXWRAP(AXUIElementCopyElementAtPosition, (AXUIElementRef el, float x, float y, AXUIElementRef *e), (el, x, y, e))

static AXError my_AXObserverCreate(pid_t p, AXObserverCallback cb, AXObserverRef *o) {
  live_log("AXObserverCreate", p > 0 && kill(p, 0) == 0 ? "pid=live" : "pid=dead");
  return AXObserverCreate(p, cb, o);
}
INTERPOSE(my_AXObserverCreate, AXObserverCreate)

static CFArrayRef my_CGWindowListCopyWindowInfo(CGWindowListOption o, CGWindowID w) { live_log("CGWindowListCopyWindowInfo", 0); return CGWindowListCopyWindowInfo(o, w); }
INTERPOSE(my_CGWindowListCopyWindowInfo, CGWindowListCopyWindowInfo)
static CFDictionaryRef my_CGSessionCopyCurrentDictionary(void) { live_log("CGSessionCopyCurrentDictionary", 0); return CGSessionCopyCurrentDictionary(); }
INTERPOSE(my_CGSessionCopyCurrentDictionary, CGSessionCopyCurrentDictionary)
static void my_CGEventPost(CGEventTapLocation t, CGEventRef e) { live_log("CGEventPost", 0); CGEventPost(t, e); }
INTERPOSE(my_CGEventPost, CGEventPost)
static void my_CGEventPostToPid(pid_t p, CGEventRef e) { live_log("CGEventPostToPid", p > 0 && kill(p, 0) == 0 ? "pid=live" : "pid=dead"); CGEventPostToPid(p, e); }
INTERPOSE(my_CGEventPostToPid, CGEventPostToPid)
static CGError my_CGWarpMouseCursorPosition(CGPoint p) { live_log("CGWarpMouseCursorPosition", 0); return CGWarpMouseCursorPosition(p); }
INTERPOSE(my_CGWarpMouseCursorPosition, CGWarpMouseCursorPosition)
static CGError my_CGAssociateMouseAndMouseCursorPosition(boolean_t c) { live_log("CGAssociateMouseAndMouseCursorPosition", 0); return CGAssociateMouseAndMouseCursorPosition(c); }
INTERPOSE(my_CGAssociateMouseAndMouseCursorPosition, CGAssociateMouseAndMouseCursorPosition)
static CFMachPortRef my_CGEventTapCreateForPid(pid_t p, CGEventTapPlacement pl, CGEventTapOptions o, CGEventMask m, CGEventTapCallBack cb, void *u) { live_log("CGEventTapCreateForPid", 0); return CGEventTapCreateForPid(p, pl, o, m, cb, u); }
INTERPOSE(my_CGEventTapCreateForPid, CGEventTapCreateForPid)
static CFTimeInterval my_CGEventSourceSecondsSinceLastEventType(CGEventSourceStateID s, CGEventType t) { live_log("CGEventSourceSecondsSinceLastEventType", 0); return CGEventSourceSecondsSinceLastEventType(s, t); }
INTERPOSE(my_CGEventSourceSecondsSinceLastEventType, CGEventSourceSecondsSinceLastEventType)
static CGEventRef my_CGEventCreate(CGEventSourceRef s) { if (!s) live_log("CGEventCreate(NULL)", "reads the live pointer"); return CGEventCreate(s); }
INTERPOSE(my_CGEventCreate, CGEventCreate)
static CGDirectDisplayID my_CGMainDisplayID(void) { live_log("CGMainDisplayID", 0); return CGMainDisplayID(); }
INTERPOSE(my_CGMainDisplayID, CGMainDisplayID)
static CGError my_CGGetActiveDisplayList(uint32_t m, CGDirectDisplayID *d, uint32_t *c) { live_log("CGGetActiveDisplayList", 0); return CGGetActiveDisplayList(m, d, c); }
INTERPOSE(my_CGGetActiveDisplayList, CGGetActiveDisplayList)
static bool my_CGPreflightScreenCaptureAccess(void) { live_log("CGPreflightScreenCaptureAccess", 0); return CGPreflightScreenCaptureAccess(); }
INTERPOSE(my_CGPreflightScreenCaptureAccess, CGPreflightScreenCaptureAccess)
static Boolean my_AXIsProcessTrusted(void) { live_log("AXIsProcessTrusted", 0); return AXIsProcessTrusted(); }
INTERPOSE(my_AXIsProcessTrusted, AXIsProcessTrusted)

static const char *main_path(void) {
  static char path[4096]; static int done = 0;
  if (!done) { uint32_t n = sizeof path; if (_NSGetExecutablePath(path, &n) != 0) path[0] = 0; done = 1; }
  return path;
}
/* Only calls made by the test binary itself (not by system frameworks). */
static int from_main(void *ret) {
  Dl_info info;
  if (!dladdr(ret, &info) || !info.dli_fname) return 0;
  const char *m = main_path();
  return m[0] && (strcmp(info.dli_fname, m) == 0 || strstr(m, info.dli_fname) || strstr(info.dli_fname, m));
}
/* SkyLight symbols that only set a field on an event object the caller owns;
 * resolving or calling them reads no live state. Every other symbol counts. */
static const char *EVENT_FIELD_SETTERS[] = {
  "CGEventSetWindowLocation", "SLEventSetIntegerValueField", NULL,
};
static int event_field_setter(const char *sym) {
  for (int i = 0; sym && EVENT_FIELD_SETTERS[i]; i++)
    if (strcmp(sym, EVENT_FIELD_SETTERS[i]) == 0) return 1;
  return 0;
}
static void *my_dlsym(void *h, const char *sym) {
  if (from_main(__builtin_return_address(0)) && !event_field_setter(sym)) live_log("dlsym", sym ? sym : "(null)");
  return dlsym(h, sym);
}
INTERPOSE(my_dlsym, dlsym)
/* A dlopen of an image that is already loaded (SkyLight comes in with
 * CoreGraphics) only returns a handle; loading a new image is logged. */
static void *my_dlopen(const char *path, int mode) {
  if (from_main(__builtin_return_address(0)) && !(path && dlopen(path, RTLD_NOLOAD | RTLD_LAZY)))
    live_log("dlopen", path ? path : "(null)");
  return dlopen(path, mode);
}
INTERPOSE(my_dlopen, dlopen)

/* AppKit class methods, swizzled once the class is loaded. */
static IMP o_shared, o_byPid, o_cursor, o_screens, o_pboard;
static id w_shared(id s, SEL c) { live_log("+[NSWorkspace sharedWorkspace]", 0); return ((id(*)(id,SEL))o_shared)(s, c); }
static id w_byPid(id s, SEL c, pid_t p) { live_log("+[NSRunningApplication runningApplicationWithProcessIdentifier:]", 0); return ((id(*)(id,SEL,pid_t))o_byPid)(s, c, p); }
static id w_cursor(id s, SEL c) { live_log("+[NSCursor currentSystemCursor]", 0); return ((id(*)(id,SEL))o_cursor)(s, c); }
static id w_screens(id s, SEL c) { live_log("+[NSScreen screens]", 0); return ((id(*)(id,SEL))o_screens)(s, c); }
static id w_pboard(id s, SEL c) { live_log("+[NSPasteboard generalPasteboard]", 0); return ((id(*)(id,SEL))o_pboard)(s, c); }
static void swz(const char *cls, const char *sel, IMP rep, IMP *orig) {
  if (*orig) return;
  Class k = objc_getClass(cls); if (!k) return;
  Method m = class_getClassMethod(k, sel_registerName(sel)); if (!m) return;
  *orig = method_setImplementation(m, rep);
}
static void try_all(void) {
  swz("NSWorkspace", "sharedWorkspace", (IMP)w_shared, &o_shared);
  swz("NSRunningApplication", "runningApplicationWithProcessIdentifier:", (IMP)w_byPid, &o_byPid);
  swz("NSCursor", "currentSystemCursor", (IMP)w_cursor, &o_cursor);
  swz("NSScreen", "screens", (IMP)w_screens, &o_screens);
  swz("NSPasteboard", "generalPasteboard", (IMP)w_pboard, &o_pboard);
}
static void on_image(const struct mach_header *mh, intptr_t s) { try_all(); }
__attribute__((constructor)) static void setup(void) {
  /* Observe this process only: child processes the tests start (sh, lsof,
   * sw_vers) must run without the probe. */
  unsetenv("DYLD_INSERT_LIBRARIES");
  _dyld_register_func_for_add_image(on_image);
  try_all();
  live_log("PROBE_READY", 0);
}
