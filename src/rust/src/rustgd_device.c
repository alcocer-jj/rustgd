// rustgd_device.c
//
// C-level graphics device for rustgd. Implements R's graphics device
// callbacks directly against pDevDesc and forwards each call into Rust
// via extern "C" functions defined in lib.rs. This bypasses extendr's
// DeviceDriver trampolines, which were causing heap corruption when
// R's display list machinery interacted with our device.
//
// Key design decisions:
//   * displayListOn = FALSE by default. R never records draws on this
//     device's display list, so recordPlot() and replayPlot() are
//     no-ops by default. ggplot, base R, and tmap workflows do not
//     depend on R's display list, so they still work. Users who
//     explicitly call dev.control("enable") opt into the display
//     list and accept whatever consequences come with it.
//   * R_GE_gcontext fields are read field-by-field at the C boundary
//     and passed to Rust as plain scalars. Rust never sees the
//     gcontext struct, never touches SEXP patternFill, never has
//     Drop or copy semantics on R's reference-counted types. This is
//     the specific mechanism intended to fix the heap corruption
//     pattern that extendr 0.9's DeviceDriver trampoline was hitting.
//   * The Rust device is owned via Box::into_raw / Box::from_raw.
//     C stores the raw pointer in dd->deviceSpecific. On close, the
//     Rust rustgd_cb_close function reconstructs the Box and lets it
//     drop, taking care of the session-dir cleanup along the way.

#define R_NO_REMAP
#include <R.h>
#include <Rinternals.h>
#include <R_ext/GraphicsEngine.h>
#include <R_ext/GraphicsDevice.h>
#include <stdlib.h>

// ----------------------------------------------------------------------
// Rust extern "C" function declarations
// ----------------------------------------------------------------------
//
// Every callback takes the Rust device pointer (stored in
// dd->deviceSpecific) as its first argument. All other arguments are
// plain scalars or pointers to plain memory. Strings come in as
// const char* (UTF-8 by R contract for textUTF8 / strWidthUTF8);
// coordinate arrays come in as const double*. Output values from
// metricInfo come back through caller-provided pointers.

extern void rustgd_cb_activate(void *dev);
extern void rustgd_cb_deactivate(void *dev);
extern void rustgd_cb_close(void *dev);
extern void rustgd_cb_new_page(void *dev, double width, double height, int fill);
extern void rustgd_cb_mode(void *dev, int mode);

extern void rustgd_cb_line(void *dev,
                           double x1, double y1, double x2, double y2,
                           int col, double lwd, int lty);

extern void rustgd_cb_polyline(void *dev,
                               int n, const double *x, const double *y,
                               int col, double lwd, int lty);

extern void rustgd_cb_rect(void *dev,
                           double x0, double y0, double x1, double y1,
                           int col, int fill, double lwd, int lty);

extern void rustgd_cb_polygon(void *dev,
                              int n, const double *x, const double *y,
                              int col, int fill, double lwd, int lty);

extern void rustgd_cb_circle(void *dev,
                             double x, double y, double r,
                             int col, int fill, double lwd, int lty);

extern void rustgd_cb_path(void *dev,
                           const double *x, const double *y,
                           int npoly, const int *nper, int winding,
                           int col, int fill, double lwd, int lty);

extern void rustgd_cb_raster(void *dev,
                             const unsigned int *raster, int w, int h,
                             double x, double y,
                             double width, double height,
                             double rot, int interpolate);

extern void rustgd_cb_text(void *dev,
                           double x, double y,
                           const char *str, double rot, double hadj,
                           int col,
                           double ps, double cex, int fontface,
                           const char *fontfamily);

extern double rustgd_cb_strwidth(void *dev,
                                 const char *str,
                                 double ps, double cex, int fontface,
                                 const char *fontfamily);

extern void rustgd_cb_char_metric(void *dev,
                                  int c,
                                  double ps, double cex, int fontface,
                                  const char *fontfamily,
                                  double *ascent, double *descent, double *width);

extern void rustgd_cb_clip(void *dev,
                           double x0, double x1, double y0, double y1);

// ----------------------------------------------------------------------
// C-level callback wrappers
// ----------------------------------------------------------------------
//
// Each wrapper unmarshals the R_GE_gcontext fields we care about and
// forwards to Rust. None of these wrappers retain pointers across
// calls; strings and arrays are valid only for the duration of the
// Rust call. None of them allocate or own anything.

static void rg_activate(pDevDesc dd) {
    rustgd_cb_activate(dd->deviceSpecific);
}

static void rg_deactivate(pDevDesc dd) {
    rustgd_cb_deactivate(dd->deviceSpecific);
}

static void rg_close(pDevDesc dd) {
    if (dd->deviceSpecific != NULL) {
        rustgd_cb_close(dd->deviceSpecific);
        dd->deviceSpecific = NULL;
    }
}

static void rg_size(double *left, double *right,
                    double *bottom, double *top, pDevDesc dd) {
    // R asks for our current device extent. dd's left/right/bottom/top
    // are the authoritative values; rustgd_set_size on the Rust side
    // updates them directly via GEcurrentDevice() when the viewer
    // resizes. No Rust round-trip needed for this query.
    *left = dd->left;
    *right = dd->right;
    *bottom = dd->bottom;
    *top = dd->top;
}

static void rg_new_page(const pGEcontext gc, pDevDesc dd) {
    double w = dd->right - dd->left;
    double h = dd->top - dd->bottom;
    rustgd_cb_new_page(dd->deviceSpecific, w, h, gc->fill);
}

static void rg_mode(int mode, pDevDesc dd) {
    rustgd_cb_mode(dd->deviceSpecific, mode);
}

static void rg_line(double x1, double y1, double x2, double y2,
                    const pGEcontext gc, pDevDesc dd) {
    rustgd_cb_line(dd->deviceSpecific, x1, y1, x2, y2,
                   gc->col, gc->lwd, gc->lty);
}

static void rg_polyline(int n, double *x, double *y,
                        const pGEcontext gc, pDevDesc dd) {
    rustgd_cb_polyline(dd->deviceSpecific, n, x, y,
                       gc->col, gc->lwd, gc->lty);
}

static void rg_rect(double x0, double y0, double x1, double y1,
                    const pGEcontext gc, pDevDesc dd) {
    rustgd_cb_rect(dd->deviceSpecific, x0, y0, x1, y1,
                   gc->col, gc->fill, gc->lwd, gc->lty);
}

static void rg_polygon(int n, double *x, double *y,
                       const pGEcontext gc, pDevDesc dd) {
    rustgd_cb_polygon(dd->deviceSpecific, n, x, y,
                      gc->col, gc->fill, gc->lwd, gc->lty);
}

static void rg_circle(double x, double y, double r,
                      const pGEcontext gc, pDevDesc dd) {
    rustgd_cb_circle(dd->deviceSpecific, x, y, r,
                     gc->col, gc->fill, gc->lwd, gc->lty);
}

static void rg_path(double *x, double *y, int npoly, int *nper,
                    Rboolean winding,
                    const pGEcontext gc, pDevDesc dd) {
    rustgd_cb_path(dd->deviceSpecific, x, y, npoly, nper, winding ? 1 : 0,
                   gc->col, gc->fill, gc->lwd, gc->lty);
}

static void rg_raster(unsigned int *raster, int w, int h,
                      double x, double y, double width, double height,
                      double rot, Rboolean interpolate,
                      const pGEcontext gc, pDevDesc dd) {
    (void)gc;  // unused; we don't apply gcontext to rasters
    rustgd_cb_raster(dd->deviceSpecific, raster, w, h, x, y,
                     width, height, rot, interpolate ? 1 : 0);
}

static void rg_text(double x, double y, const char *str,
                    double rot, double hadj,
                    const pGEcontext gc, pDevDesc dd) {
    rustgd_cb_text(dd->deviceSpecific, x, y, str, rot, hadj,
                   gc->col, gc->ps, gc->cex, gc->fontface, gc->fontfamily);
}

static double rg_strwidth(const char *str,
                          const pGEcontext gc, pDevDesc dd) {
    return rustgd_cb_strwidth(dd->deviceSpecific, str,
                              gc->ps, gc->cex, gc->fontface, gc->fontfamily);
}

static void rg_char_metric(int c, const pGEcontext gc,
                           double *ascent, double *descent, double *width,
                           pDevDesc dd) {
    rustgd_cb_char_metric(dd->deviceSpecific, c,
                          gc->ps, gc->cex, gc->fontface, gc->fontfamily,
                          ascent, descent, width);
}

static void rg_clip(double x0, double x1, double y0, double y1, pDevDesc dd) {
    rustgd_cb_clip(dd->deviceSpecific, x0, x1, y0, y1);
}

// ----------------------------------------------------------------------
// No-op stubs for R 4.1+ optional callbacks
// ----------------------------------------------------------------------
//
// R's grid graphics calls these hooks unconditionally during normal
// operation (for example, grid.newpage() invokes setMask(NULL, NULL)
// to reset mask state, even when the plot uses no masks). The engine
// does not always gate these calls on dev->deviceVersion, so any
// function pointer left NULL will crash with a NULL-pointer call.
//
// All of these return R_NilValue (for SEXP returns) or do nothing
// (for void returns), which is the conventional "this device declines
// to render the feature" signal. Grid degrades gracefully to fallback
// rendering paths. This matches svglite's approach for features it
// chooses not to implement.

static SEXP rg_set_pattern(SEXP pattern, pDevDesc dd) {
    (void)pattern;
    (void)dd;
    return R_NilValue;
}

static void rg_release_pattern(SEXP ref, pDevDesc dd) {
    (void)ref;
    (void)dd;
}

static SEXP rg_set_clip_path(SEXP path, SEXP ref, pDevDesc dd) {
    (void)path;
    (void)ref;
    (void)dd;
    return R_NilValue;
}

static void rg_release_clip_path(SEXP ref, pDevDesc dd) {
    (void)ref;
    (void)dd;
}

static SEXP rg_set_mask(SEXP path, SEXP ref, pDevDesc dd) {
    (void)path;
    (void)ref;
    (void)dd;
    return R_NilValue;
}

static void rg_release_mask(SEXP ref, pDevDesc dd) {
    (void)ref;
    (void)dd;
}

static SEXP rg_define_group(SEXP source, int op, SEXP destination, pDevDesc dd) {
    (void)source;
    (void)op;
    (void)destination;
    (void)dd;
    return R_NilValue;
}

static void rg_use_group(SEXP ref, SEXP trans, pDevDesc dd) {
    (void)ref;
    (void)trans;
    (void)dd;
}

static void rg_release_group(SEXP ref, pDevDesc dd) {
    (void)ref;
    (void)dd;
}

static void rg_stroke(SEXP path, const pGEcontext gc, pDevDesc dd) {
    (void)path;
    (void)gc;
    (void)dd;
}

static void rg_fill(SEXP path, int rule, const pGEcontext gc, pDevDesc dd) {
    (void)path;
    (void)rule;
    (void)gc;
    (void)dd;
}

static void rg_fill_stroke(SEXP path, int rule, const pGEcontext gc, pDevDesc dd) {
    (void)path;
    (void)rule;
    (void)gc;
    (void)dd;
}

static SEXP rg_capabilities(SEXP cap) {
    // Return the capability list unchanged. R will use its defaults,
    // which is correct for a device that supports neither patterns,
    // masks, clip paths, nor groups.
    return cap;
}

static void rg_glyph(int n, int *glyphs, double *x, double *y,
                     SEXP font, double size,
                     int colour, double rot,
                     pDevDesc dd) {
    (void)n;
    (void)glyphs;
    (void)x;
    (void)y;
    (void)font;
    (void)size;
    (void)colour;
    (void)rot;
    (void)dd;
    // Grid's glyph rendering: we decline, so text falls back to the
    // regular text() callback (which we do implement).
}

// ----------------------------------------------------------------------
// Device registration
// ----------------------------------------------------------------------
//
// Called from Rust's rustgd_activate after the Rust device has been
// constructed and boxed. Allocates a pDevDesc, populates capability
// fields and callback pointers, and registers with R's graphics
// engine via GEcreateDevDesc + GEaddDevice2.
//
// Returns 0 on success, nonzero on failure. On nonzero return, the
// caller (Rust) is responsible for dropping the Rust device, since
// dd->deviceSpecific was never adopted by R.

// Update the current device's stored extent and clip region. Called
// from Rust's rustgd_set_size when the viewer reports a resize. We
// touch only the well-defined leading fields of DevDesc that have
// been stable across every R version since the graphics engine API
// was introduced. Doing this in C avoids reproducing pDevDesc's
// layout in Rust, which would be fragile across R updates.

void rustgd_set_device_size(double width, double height) {
    pGEDevDesc gdd = GEcurrentDevice();
    if (gdd == NULL) {
        return;
    }
    pDevDesc dd = gdd->dev;
    if (dd == NULL) {
        return;
    }
    dd->left = 0.0;
    dd->right = width;
    dd->bottom = 0.0;
    dd->top = height;
    dd->clipLeft = 0.0;
    dd->clipRight = width;
    dd->clipBottom = 0.0;
    dd->clipTop = height;
}

int rustgd_register_device(void *rust_dev,
                           double width, double height,
                           const char *name) {
    pDevDesc dev;
    pGEDevDesc gdd;

    R_GE_checkVersionOrDie(R_GE_version);
    R_CheckDeviceAvailable();

    dev = (pDevDesc) calloc(1, sizeof(DevDesc));
    if (dev == NULL) {
        return 1;
    }

    BEGIN_SUSPEND_INTERRUPTS {
        // Device extent
        dev->left = 0.0;
        dev->right = width;
        dev->bottom = 0.0;
        dev->top = height;
        dev->clipLeft = 0.0;
        dev->clipRight = width;
        dev->clipBottom = 0.0;
        dev->clipTop = height;

        // Text positioning hints. These match svglite and ragg
        // conventions for screen-style devices.
        dev->xCharOffset = 0.4900;
        dev->yCharOffset = 0.3333;
        dev->yLineBias = 0.2;

        // Inches per raster pixel: 96 DPI assumption matches the
        // viewer's supersampled rasterization at SUPERSAMPLE = 2.0.
        dev->ipr[0] = 1.0 / 96.0;
        dev->ipr[1] = 1.0 / 96.0;

        // Character size in raster pixels at the 12pt default
        // pointsize. 0.9 * ps for width and 1.2 * ps for height
        // matches svglite. R scales these by gc->cex at draw time.
        dev->cra[0] = 0.9 * 12.0;
        dev->cra[1] = 1.2 * 12.0;

        dev->gamma = 1.0;
        dev->canClip = TRUE;
        dev->canChangeGamma = FALSE;
        dev->canHAdj = 1;  // 0=none, 1=left/center/right, 2=continuous

        dev->startps = 12.0;
        dev->startcol = R_RGB(0, 0, 0);
        dev->startfill = R_TRANWHITE;
        dev->startlty = LTY_SOLID;
        dev->startfont = 1;
        dev->startgamma = 1.0;

        // Critical: keep R's display list OFF by default. This means
        // recordPlot/replayPlot capture nothing for our device and
        // therefore can't drive R's display-list-replay machinery
        // through our callbacks. ggplot, base R, and tmap render
        // fine because none of them depend on the display list.
        dev->displayListOn = FALSE;

        // No event handling.
        dev->canGenMouseDown = FALSE;
        dev->canGenMouseMove = FALSE;
        dev->canGenMouseUp = FALSE;
        dev->canGenKeybd = FALSE;
        dev->gettingEvent = FALSE;

        // Capability flags. R uses 1=no, 2=yes for these tri-state
        // capabilities. We support transparency in colors, transparent
        // backgrounds, and raster output. We do not implement screen
        // capture or interactive locator.
        dev->haveTransparency = 2;
        dev->haveTransparentBg = 2;
        dev->haveRaster = 2;
        dev->haveCapture = 1;
        dev->haveLocator = 1;

        // UTF-8 text. Our text callback is the canonical text handler;
        // textUTF8 points at the same function. R 4.x and later guarantee
        // UTF-8 strings to textUTF8 / strWidthUTF8.
        dev->hasTextUTF8 = TRUE;
        dev->wantSymbolUTF8 = TRUE;
        dev->useRotatedTextInContour = FALSE;

        // Device version. We advertise full current-engine support so
        // grid will route all calls through our callbacks; the stubs
        // declared above handle features we choose not to implement
        // by returning R_NilValue or doing nothing. Setting this to
        // R_GE_version keeps us in sync with whatever R we built
        // against, matching svglite and ragg conventions.
        dev->deviceVersion = R_GE_version;
        dev->deviceClip = FALSE;  // rectangular clip via dev->clip only

        // Hook callbacks
        dev->activate = rg_activate;
        dev->deactivate = rg_deactivate;
        dev->close = rg_close;
        dev->size = rg_size;
        dev->newPage = rg_new_page;
        dev->clip = rg_clip;
        dev->strWidth = rg_strwidth;
        dev->text = rg_text;
        dev->rect = rg_rect;
        dev->line = rg_line;
        dev->circle = rg_circle;
        dev->polyline = rg_polyline;
        dev->polygon = rg_polygon;
        dev->path = rg_path;
        dev->raster = rg_raster;
        dev->metricInfo = rg_char_metric;
        dev->mode = rg_mode;
        dev->textUTF8 = rg_text;
        dev->strWidthUTF8 = rg_strwidth;

        // Pattern, clip-path, mask, group, stroke/fill, and glyph
        // hooks. We provide no-op stubs (see above) so grid's
        // unconditional calls to setMask(NULL, NULL) and similar do
        // not crash on a NULL function pointer.
        dev->setPattern = rg_set_pattern;
        dev->releasePattern = rg_release_pattern;
        dev->setClipPath = rg_set_clip_path;
        dev->releaseClipPath = rg_release_clip_path;
        dev->setMask = rg_set_mask;
        dev->releaseMask = rg_release_mask;
        dev->defineGroup = rg_define_group;
        dev->useGroup = rg_use_group;
        dev->releaseGroup = rg_release_group;
        dev->stroke = rg_stroke;
        dev->fill = rg_fill;
        dev->fillStroke = rg_fill_stroke;
        dev->capabilities = rg_capabilities;
        dev->glyph = rg_glyph;

        // Callbacks we do not implement. R checks for NULL and skips
        // these gracefully (unlike the R 4.1+ hooks above, which can
        // be invoked unconditionally).
        dev->locator = NULL;
        dev->cap = NULL;
        dev->onExit = NULL;
        dev->getEvent = NULL;
        dev->newFrameConfirm = NULL;
        dev->eventEnv = R_NilValue;
        dev->eventHelper = NULL;
        dev->holdflush = NULL;

        // Stash our Rust device pointer.
        dev->deviceSpecific = rust_dev;

        gdd = GEcreateDevDesc(dev);
        GEaddDevice2(gdd, name);
    } END_SUSPEND_INTERRUPTS;

    return 0;
}
