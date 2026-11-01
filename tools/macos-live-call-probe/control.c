// Positive control for the live-call probe (goat-si/cua-driver#188): calls each
// read-only family the probe intercepts exactly once, so the CI step can prove
// interposition works on the runner. Input posting and event taps are left out
// on purpose: posting events on a shared runner is not safe. Link it with
// AppKit so its classes exist when the probe loads:
//   clang -o control control.c -framework ApplicationServices -framework AppKit -lobjc
#include <ApplicationServices/ApplicationServices.h>
#include <dlfcn.h>
#include <objc/message.h>
#include <objc/runtime.h>
#include <stdio.h>

static void class_call(const char *cls, const char *sel) {
  Class k = objc_getClass(cls);
  if (!k) { fprintf(stderr, "control: no class %s\n", cls); return; }
  ((id (*)(id, SEL))objc_msgSend)((id)k, sel_registerName(sel));
}

int main(void) {
  AXUIElementRef app = AXUIElementCreateApplication(1);
  AXUIElementSetMessagingTimeout(app, 0.2f);
  CFTypeRef value = NULL;
  AXUIElementCopyAttributeValue(app, kAXRoleAttribute, &value);
  if (value) CFRelease(value);
  CFRelease(app);
  (void)AXIsProcessTrusted();
  CFArrayRef windows = CGWindowListCopyWindowInfo(kCGWindowListOptionOnScreenOnly, kCGNullWindowID);
  if (windows) CFRelease(windows);
  CFDictionaryRef session = CGSessionCopyCurrentDictionary();
  if (session) CFRelease(session);
  (void)CGEventSourceSecondsSinceLastEventType(kCGEventSourceStateHIDSystemState, kCGEventKeyDown);
  CGEventRef pointer = CGEventCreate(NULL);
  if (pointer) CFRelease(pointer);
  (void)CGMainDisplayID();
  uint32_t count = 0;
  (void)CGGetActiveDisplayList(0, NULL, &count);
  (void)CGPreflightScreenCaptureAccess();
  /* A load of an image that is not already loaded is logged; this one does
   * not exist, so nothing is loaded. */
  (void)dlopen("/usr/lib/cua-live-call-probe-control-missing.dylib", RTLD_LAZY);
  (void)dlsym(RTLD_DEFAULT, "SLSMainConnectionID");
  class_call("NSWorkspace", "sharedWorkspace");
  Class running = objc_getClass("NSRunningApplication");
  if (running)
    ((id (*)(id, SEL, pid_t))objc_msgSend)((id)running, sel_registerName("runningApplicationWithProcessIdentifier:"), getpid());
  class_call("NSCursor", "currentSystemCursor");
  class_call("NSScreen", "screens");
  class_call("NSPasteboard", "generalPasteboard");
  return 0;
}
