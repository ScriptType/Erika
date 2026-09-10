#import <AppKit/AppKit.h>
#import <QuartzCore/QuartzCore.h>
#import <math.h>
#import <stdlib.h>

extern void erika_demo_attach_layer(void *layer, unsigned int width, unsigned int height, double scale);
extern void erika_demo_resize_layer(unsigned int width, unsigned int height, double scale);
extern void erika_demo_render_frame(double time_seconds);
extern void erika_demo_update_headroom(double headroom);
extern bool erika_demo_close_ready(void);
extern void erika_demo_toggle_play_pause(void);
extern void erika_demo_seek_seconds(double seconds);
extern double erika_demo_position_seconds(void);
extern double erika_demo_duration_seconds(void);
extern bool erika_demo_is_playing(void);
extern double erika_demo_smoke_seconds(void);

static NSString *ErikaFormatTime(double seconds) {
  if (!isfinite(seconds) || seconds < 0.0) {
    seconds = 0.0;
  }
  NSInteger total = (NSInteger)llround(seconds);
  NSInteger hours = total / 3600;
  NSInteger minutes = (total / 60) % 60;
  NSInteger secs = total % 60;
  if (hours > 0) {
    return [NSString stringWithFormat:@"%ld:%02ld:%02ld", (long)hours, (long)minutes, (long)secs];
  }
  return [NSString stringWithFormat:@"%ld:%02ld", (long)minutes, (long)secs];
}

@interface ErikaMetalDemoView : NSView
@property(nonatomic, strong) CAMetalLayer *metalLayer;
@property(nonatomic, strong) NSTimer *timer;
@property(nonatomic, assign) CFTimeInterval startTime;
@property(nonatomic, assign) CFTimeInterval lastWindowLog;
@property(nonatomic, strong) NSDictionary *lastWindowState;
@property(nonatomic, assign) BOOL diagnosticsEnabled;
@end

@implementation ErikaMetalDemoView

- (instancetype)initWithFrame:(NSRect)frameRect {
  self = [super initWithFrame:frameRect];
  if (self) {
    self.wantsLayer = YES;
    self.metalLayer = [CAMetalLayer layer];
    self.metalLayer.pixelFormat = MTLPixelFormatBGRA8Unorm;
    self.metalLayer.framebufferOnly = YES;
    self.metalLayer.opaque = YES;
    self.layer = self.metalLayer;
    self.startTime = CACurrentMediaTime();
    self.diagnosticsEnabled = getenv("ERIKA_ADAPTER_DIAGNOSTICS") &&
      strcmp(getenv("ERIKA_ADAPTER_DIAGNOSTICS"), "1") == 0;
  }
  return self;
}

- (BOOL)wantsUpdateLayer {
  return YES;
}

- (void)viewDidMoveToWindow {
  [super viewDidMoveToWindow];
  [self updateDrawableSizeAndAttach:YES];
  if (self.diagnosticsEnabled && self.window != nil) {
    NSNotificationCenter *center = NSNotificationCenter.defaultCenter;
    for (NSNotificationName name in @[NSWindowDidChangeOcclusionStateNotification,
        NSWindowDidMiniaturizeNotification, NSWindowDidDeminiaturizeNotification,
        NSWindowDidBecomeKeyNotification, NSWindowDidResignKeyNotification]) {
      [center addObserver:self selector:@selector(diagnosticWindowChanged:) name:name object:self.window];
    }
    for (NSNotificationName name in @[NSApplicationDidBecomeActiveNotification,
        NSApplicationDidResignActiveNotification, NSApplicationDidHideNotification,
        NSApplicationDidUnhideNotification]) {
      [center addObserver:self selector:@selector(diagnosticWindowChanged:) name:name object:NSApp];
    }
    [self recordWindowState];
  }
  if (self.window != nil && self.timer == nil) {
    self.timer = [NSTimer scheduledTimerWithTimeInterval:(1.0 / 60.0)
                                                  target:self
                                                selector:@selector(renderTick:)
                                                userInfo:nil
                                                 repeats:YES];
    [[NSRunLoop mainRunLoop] addTimer:self.timer forMode:NSRunLoopCommonModes];
  }
}

- (void)viewWillMoveToWindow:(NSWindow *)newWindow {
  if (self.diagnosticsEnabled) {
    [NSNotificationCenter.defaultCenter removeObserver:self];
  }
  if (newWindow == nil) {
    [self.timer invalidate];
    self.timer = nil;
  }
  [super viewWillMoveToWindow:newWindow];
}

- (void)setFrameSize:(NSSize)newSize {
  [super setFrameSize:newSize];
  [self updateDrawableSizeAndAttach:NO];
}

- (void)viewDidChangeBackingProperties {
  [super viewDidChangeBackingProperties];
  [self updateDrawableSizeAndAttach:NO];
}

- (void)updateDrawableSizeAndAttach:(BOOL)attach {
  CGFloat scale = self.window.backingScaleFactor > 0 ? self.window.backingScaleFactor : NSScreen.mainScreen.backingScaleFactor;
  CGSize drawableSize = CGSizeMake(MAX(1.0, self.bounds.size.width * scale), MAX(1.0, self.bounds.size.height * scale));
  self.metalLayer.drawableSize = drawableSize;
  self.metalLayer.frame = self.bounds;
  unsigned int pixelWidth = (unsigned int)MAX(1.0, round(drawableSize.width));
  unsigned int pixelHeight = (unsigned int)MAX(1.0, round(drawableSize.height));
  if (attach) {
    erika_demo_attach_layer((__bridge void *)self.metalLayer, pixelWidth, pixelHeight, scale);
  } else {
    erika_demo_resize_layer(pixelWidth, pixelHeight, scale);
  }
}

- (void)renderTick:(NSTimer *)timer {
  (void)timer;
  double elapsed = CACurrentMediaTime() - self.startTime;
  if (self.diagnosticsEnabled) {
    [self recordWindowState];
  }
  erika_demo_update_headroom(self.window.screen.maximumExtendedDynamicRangeColorComponentValue);
  erika_demo_render_frame(elapsed);
}

- (void)recordWindowState {
  NSWindow *window = self.window;
  NSDictionary *state = @{
    @"active": @(NSApp.isActive), @"key": @(window.isKeyWindow),
    @"visible": @(window.isVisible), @"onActiveSpace": @(window.isOnActiveSpace),
    @"occlusionVisible": @((window.occlusionState & NSWindowOcclusionStateVisible) != 0),
    @"miniaturized": @(window.isMiniaturized), @"windowNumber": @(window.windowNumber),
    @"frontmostPID": @(NSWorkspace.sharedWorkspace.frontmostApplication.processIdentifier),
    @"pid": @(NSProcessInfo.processInfo.processIdentifier),
    @"layerMatchesView": @(self.layer == self.metalLayer),
    @"layerHasSuperlayer": @(self.metalLayer.superlayer != nil),
    @"layerHidden": @(self.metalLayer.hidden), @"layerOpacity": @(self.metalLayer.opacity),
    @"presentsWithTransaction": @(self.metalLayer.presentsWithTransaction),
    @"displaySyncEnabled": @(self.metalLayer.displaySyncEnabled),
    @"drawableWidth": @(self.metalLayer.drawableSize.width),
    @"drawableHeight": @(self.metalLayer.drawableSize.height),
    @"contentsScale": @(self.metalLayer.contentsScale),
    @"bounds": NSStringFromRect(self.bounds), @"visibleRect": NSStringFromRect(self.visibleRect)
  };
  CFTimeInterval now = CACurrentMediaTime();
  if (![state isEqualToDictionary:self.lastWindowState] || now - self.lastWindowLog >= 1.0) {
    NSDictionary *event = @{@"event": @"erika_native_window", @"host": @(now),
      @"elapsed": @(now - self.startTime), @"state": state,
      @"edrHeadroom": @(window.screen.maximumExtendedDynamicRangeColorComponentValue)};
    NSData *json = [NSJSONSerialization dataWithJSONObject:event options:0 error:nil];
    fprintf(stderr, "%s\n", [[NSString alloc] initWithData:json encoding:NSUTF8StringEncoding].UTF8String);
    self.lastWindowState = state;
    self.lastWindowLog = now;
  }
}

- (void)diagnosticWindowChanged:(NSNotification *)notification {
  (void)notification;
  [self recordWindowState];
}

@end

@interface ErikaControlsView : NSView
@property(nonatomic, strong) NSButton *playPauseButton;
@property(nonatomic, strong) NSSlider *progressSlider;
@property(nonatomic, strong) NSTextField *timeLabel;
@property(nonatomic, strong) NSTimer *timer;
@property(nonatomic, assign) BOOL scrubbing;
@end

@implementation ErikaControlsView

- (instancetype)initWithFrame:(NSRect)frameRect {
  self = [super initWithFrame:frameRect];
  if (self) {
    self.wantsLayer = YES;
    self.layer.backgroundColor = [NSColor colorWithWhite:0.08 alpha:1.0].CGColor;

    self.playPauseButton = [NSButton buttonWithTitle:@"Pause" target:self action:@selector(togglePlayPause:)];
    self.playPauseButton.bezelStyle = NSBezelStyleRegularSquare;
    self.playPauseButton.bordered = NO;
    self.playPauseButton.wantsLayer = YES;
    self.playPauseButton.layer.backgroundColor = [NSColor colorWithWhite:0.18 alpha:1.0].CGColor;
    self.playPauseButton.layer.cornerRadius = 4.0;
    self.playPauseButton.contentTintColor = NSColor.whiteColor;
    self.playPauseButton.translatesAutoresizingMaskIntoConstraints = NO;
    [self addSubview:self.playPauseButton];

    self.progressSlider = [[NSSlider alloc] initWithFrame:NSZeroRect];
    self.progressSlider.minValue = 0.0;
    self.progressSlider.maxValue = 1.0;
    self.progressSlider.doubleValue = 0.0;
    self.progressSlider.continuous = YES;
    self.progressSlider.target = self;
    self.progressSlider.action = @selector(sliderChanged:);
    self.progressSlider.translatesAutoresizingMaskIntoConstraints = NO;
    [self addSubview:self.progressSlider];

    self.timeLabel = [NSTextField labelWithString:@"0:00 / 0:00"];
    self.timeLabel.textColor = NSColor.whiteColor;
    self.timeLabel.alignment = NSTextAlignmentRight;
    self.timeLabel.font = [NSFont monospacedDigitSystemFontOfSize:12.0 weight:NSFontWeightRegular];
    self.timeLabel.translatesAutoresizingMaskIntoConstraints = NO;
    [self addSubview:self.timeLabel];

    [NSLayoutConstraint activateConstraints:@[
      [self.playPauseButton.leadingAnchor constraintEqualToAnchor:self.leadingAnchor constant:12.0],
      [self.playPauseButton.centerYAnchor constraintEqualToAnchor:self.centerYAnchor],
      [self.playPauseButton.widthAnchor constraintEqualToConstant:76.0],
      [self.progressSlider.leadingAnchor constraintEqualToAnchor:self.playPauseButton.trailingAnchor constant:12.0],
      [self.progressSlider.trailingAnchor constraintEqualToAnchor:self.timeLabel.leadingAnchor constant:-12.0],
      [self.progressSlider.centerYAnchor constraintEqualToAnchor:self.centerYAnchor],
      [self.timeLabel.trailingAnchor constraintEqualToAnchor:self.trailingAnchor constant:-12.0],
      [self.timeLabel.centerYAnchor constraintEqualToAnchor:self.centerYAnchor],
      [self.timeLabel.widthAnchor constraintEqualToConstant:118.0],
    ]];
  }
  return self;
}

- (void)viewDidMoveToWindow {
  [super viewDidMoveToWindow];
  if (self.window != nil && self.timer == nil) {
    self.timer = [NSTimer scheduledTimerWithTimeInterval:0.25
                                                  target:self
                                                selector:@selector(refreshControls:)
                                                userInfo:nil
                                                 repeats:YES];
    [[NSRunLoop mainRunLoop] addTimer:self.timer forMode:NSRunLoopCommonModes];
    [self refreshControls:nil];
  }
}

- (void)viewWillMoveToWindow:(NSWindow *)newWindow {
  if (newWindow == nil) {
    [self.timer invalidate];
    self.timer = nil;
  }
  [super viewWillMoveToWindow:newWindow];
}

- (void)togglePlayPause:(id)sender {
  (void)sender;
  erika_demo_toggle_play_pause();
  [self refreshControls:nil];
}

- (void)sliderChanged:(NSSlider *)sender {
  double duration = erika_demo_duration_seconds();
  if (duration <= 0.0 || !isfinite(duration)) {
    return;
  }
  self.scrubbing = YES;
  erika_demo_seek_seconds(sender.doubleValue);
  [self refreshControls:nil];
  self.scrubbing = NO;
}

- (void)refreshControls:(NSTimer *)timer {
  (void)timer;
  double duration = erika_demo_duration_seconds();
  double position = erika_demo_position_seconds();
  BOOL hasDuration = duration > 0.0 && isfinite(duration);
  if (hasDuration) {
    self.progressSlider.enabled = YES;
    self.progressSlider.maxValue = duration;
    if (!self.scrubbing) {
      self.progressSlider.doubleValue = MIN(MAX(position, 0.0), duration);
    }
  } else {
    self.progressSlider.enabled = NO;
    self.progressSlider.maxValue = 1.0;
    self.progressSlider.doubleValue = 0.0;
  }
  self.playPauseButton.title = erika_demo_is_playing() ? @"Pause" : @"Play";
  self.timeLabel.stringValue = [NSString stringWithFormat:@"%@ / %@", ErikaFormatTime(position), ErikaFormatTime(duration)];
}

@end

@interface ErikaPlayerContainerView : NSView
@property(nonatomic, strong) ErikaMetalDemoView *videoView;
@property(nonatomic, strong) ErikaControlsView *controlsView;
@end

@implementation ErikaPlayerContainerView

- (instancetype)initWithFrame:(NSRect)frameRect {
  self = [super initWithFrame:frameRect];
  if (self) {
    self.wantsLayer = YES;
    self.layer.backgroundColor = NSColor.blackColor.CGColor;

    self.videoView = [[ErikaMetalDemoView alloc] initWithFrame:NSZeroRect];
    self.videoView.translatesAutoresizingMaskIntoConstraints = NO;
    [self addSubview:self.videoView];

    self.controlsView = [[ErikaControlsView alloc] initWithFrame:NSZeroRect];
    self.controlsView.translatesAutoresizingMaskIntoConstraints = NO;
    [self addSubview:self.controlsView];

    [NSLayoutConstraint activateConstraints:@[
      [self.videoView.leadingAnchor constraintEqualToAnchor:self.leadingAnchor],
      [self.videoView.trailingAnchor constraintEqualToAnchor:self.trailingAnchor],
      [self.videoView.topAnchor constraintEqualToAnchor:self.topAnchor],
      [self.videoView.bottomAnchor constraintEqualToAnchor:self.controlsView.topAnchor],
      [self.controlsView.leadingAnchor constraintEqualToAnchor:self.leadingAnchor],
      [self.controlsView.trailingAnchor constraintEqualToAnchor:self.trailingAnchor],
      [self.controlsView.bottomAnchor constraintEqualToAnchor:self.bottomAnchor],
      [self.controlsView.heightAnchor constraintEqualToConstant:44.0],
    ]];
  }
  return self;
}

@end

@interface ErikaMetalDemoDelegate : NSObject <NSApplicationDelegate>
@property(nonatomic, strong) NSWindow *window;
@property(nonatomic, strong) NSTimer *smokeTimer;
@property(nonatomic, strong) NSTimer *diagnosticCoverTimer;
@property(nonatomic, strong) NSTimer *diagnosticRevealTimer;
@property(nonatomic, strong) NSWindow *diagnosticCoverWindow;
@end

@implementation ErikaMetalDemoDelegate

- (void)applicationDidFinishLaunching:(NSNotification *)notification {
  (void)notification;
  NSRect frame = NSMakeRect(0, 0, 960, 540);
  const char *widthText = getenv("ERIKA_ADAPTER_DISPLAY_WIDTH");
  const char *heightText = getenv("ERIKA_ADAPTER_DISPLAY_HEIGHT");
  if (widthText || heightText) {
    char *widthEnd = NULL, *heightEnd = NULL;
    long width = widthText ? strtol(widthText, &widthEnd, 10) : 0;
    long height = heightText ? strtol(heightText, &heightEnd, 10) : 0;
    if (!widthEnd || *widthEnd || !heightEnd || *heightEnd ||
        width < 640 || width > 8192 || height < 360 || height > 8192) {
      NSLog(@"Invalid requested physical drawable size");
      exit(64);
    }
    CGFloat scale = NSScreen.mainScreen.backingScaleFactor;
    if (scale <= 0) scale = 1;
    frame.size = NSMakeSize((CGFloat)width / scale, (CGFloat)height / scale + 44);
  }
  self.window = [[NSWindow alloc] initWithContentRect:frame
                                            styleMask:(NSWindowStyleMaskTitled |
                                                       NSWindowStyleMaskClosable |
                                                       NSWindowStyleMaskMiniaturizable |
                                                       NSWindowStyleMaskResizable)
                                              backing:NSBackingStoreBuffered
                                                defer:NO];
  self.window.title = @"Erika Metal Demo";
  self.window.contentView = [[ErikaPlayerContainerView alloc] initWithFrame:frame];
  [self.window center];
  [self.window makeKeyAndOrderFront:nil];
  if (getenv("ERIKA_ADAPTER_FOREGROUND") && strcmp(getenv("ERIKA_ADAPTER_FOREGROUND"), "1") == 0) {
    [NSApp activateIgnoringOtherApps:YES];
  }
  // Diagnostic-only occlusion leaves the playback window and all renderer
  // timers alive. Ordering out the application's final window may terminate it.
  if (getenv("ERIKA_ADAPTER_DIAGNOSTICS") && strcmp(getenv("ERIKA_ADAPTER_DIAGNOSTICS"), "1") == 0 &&
      getenv("ERIKA_ADAPTER_OCCLUDE_AT") && getenv("ERIKA_ADAPTER_REVEAL_AT")) {
    self.diagnosticCoverTimer = [NSTimer scheduledTimerWithTimeInterval:strtod(getenv("ERIKA_ADAPTER_OCCLUDE_AT"), NULL)
      repeats:NO block:^(NSTimer *timer) {
        (void)timer;
        self.diagnosticCoverWindow = [[NSWindow alloc] initWithContentRect:self.window.frame
          styleMask:NSWindowStyleMaskBorderless backing:NSBackingStoreBuffered defer:NO];
        self.diagnosticCoverWindow.backgroundColor = NSColor.blackColor;
        self.diagnosticCoverWindow.opaque = YES;
        self.diagnosticCoverWindow.hasShadow = NO;
        self.diagnosticCoverWindow.ignoresMouseEvents = YES;
        self.diagnosticCoverWindow.level = self.window.level + 1;
        [self.diagnosticCoverWindow orderFrontRegardless];
      }];
    self.diagnosticRevealTimer = [NSTimer scheduledTimerWithTimeInterval:strtod(getenv("ERIKA_ADAPTER_REVEAL_AT"), NULL)
      repeats:NO block:^(NSTimer *timer) {
        (void)timer;
        [self.diagnosticCoverWindow orderOut:nil];
        self.diagnosticCoverWindow = nil;
      }];
    [[NSRunLoop mainRunLoop] addTimer:self.diagnosticCoverTimer forMode:NSRunLoopCommonModes];
    [[NSRunLoop mainRunLoop] addTimer:self.diagnosticRevealTimer forMode:NSRunLoopCommonModes];
  }
  double smokeSeconds = erika_demo_smoke_seconds();
  if (smokeSeconds > 0.0) {
    self.smokeTimer = [NSTimer scheduledTimerWithTimeInterval:smokeSeconds
                                                       target:self
                                                     selector:@selector(smokeTimerFired:)
                                                     userInfo:nil
                                                      repeats:NO];
    [[NSRunLoop mainRunLoop] addTimer:self.smokeTimer forMode:NSRunLoopCommonModes];
  } else {
    [NSApp activateIgnoringOtherApps:YES];
  }
}

- (void)smokeTimerFired:(NSTimer *)timer {
  (void)timer;
  [NSApp terminate:nil];
}

- (NSApplicationTerminateReply)applicationShouldTerminate:(NSApplication *)sender {
  (void)sender;
  if (erika_demo_close_ready()) return NSTerminateNow;
  NSTimer *drainTimer = [NSTimer timerWithTimeInterval:0.02 repeats:YES block:^(NSTimer *timer) {
    if (erika_demo_close_ready()) { [timer invalidate]; [NSApp replyToApplicationShouldTerminate:YES]; }
  }];
  // NSTerminateLater runs AppKit's modal loop, not the normal display loop.
  [[NSRunLoop mainRunLoop] addTimer:drainTimer forMode:NSRunLoopCommonModes];
  [[NSRunLoop mainRunLoop] addTimer:drainTimer forMode:NSModalPanelRunLoopMode];
  return NSTerminateLater;
}

- (BOOL)applicationShouldTerminateAfterLastWindowClosed:(NSApplication *)sender {
  (void)sender;
  return YES;
}

@end

void erika_demo_run_app(void) {
  @autoreleasepool {
    NSApplication *app = [NSApplication sharedApplication];
    app.activationPolicy = NSApplicationActivationPolicyRegular;
    ErikaMetalDemoDelegate *delegate = [[ErikaMetalDemoDelegate alloc] init];
    app.delegate = delegate;
    [app run];
  }
}
