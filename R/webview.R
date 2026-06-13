#' Use the rustgd web viewer for HTML content.
#'
#' Registers the rustgd webview window as R's HTML viewer by setting
#' `options(viewer = ...)`, and also routes Shiny apps to the same window by
#' setting `options(shiny.launch.browser = ...)`. After this, printing an
#' htmlwidget (plotly, leaflet, a tmap in view mode, and so on) at the
#' console, or running a Shiny app with [shiny::runApp()], opens in a rustgd
#' webview window instead of a browser tab. This is meant for terminal and
#' editor R sessions (radian, Zed, Neovim) that otherwise have no viewer pane.
#'
#' The window launches lazily: it opens the first time you display something,
#' not when this function is called. Later content reuses the same window.
#' Closing the window is fine; the next item reopens one.
#'
#' Widgets are copied into a per-session directory under the system temp
#' folder, so what the window shows does not depend on R's own temp files.
#' Shiny apps and other live URLs are shown in place by pointing the window at
#' the running address; see [rustgd_browse()]. For a Shiny app, stopping it
#' (Ctrl+C, which is also what returns your terminal) removes its entry from
#' the window automatically, so no separate clear step is needed. Closing the
#' window deletes the copies, and quitting R removes the whole directory, so
#' nothing is left behind.
#'
#' The previous values of `options("viewer")` and
#' `options("shiny.launch.browser")` are remembered and restored by
#' [unuse_rustgd_webview()].
#'
#' @export
use_rustgd_webview <- function() {
  prev_viewer <- getOption("viewer")
  # Avoid stashing our own hook as the "previous" viewer when called twice.
  if (!identical(prev_viewer, rustgd_view)) {
    .rustgd_state$prev_viewer <- prev_viewer
  }
  options(viewer = rustgd_view)

  prev_launch <- getOption("shiny.launch.browser")
  if (!identical(prev_launch, rustgd_shiny_launch)) {
    .rustgd_state$prev_launch_browser <- prev_launch
  }
  options(shiny.launch.browser = rustgd_shiny_launch)

  invisible(NULL)
}

#' Stop using the rustgd web viewer.
#'
#' Inverse of [use_rustgd_webview()]. Restores whatever `options("viewer")`
#' and `options("shiny.launch.browser")` were set before, so an IDE viewer
#' pane (RStudio, Positron) and the default Shiny launcher take over again.
#' Any open rustgd webview window is left alone.
#'
#' @export
unuse_rustgd_webview <- function() {
  options(viewer = .rustgd_state$prev_viewer)
  options(shiny.launch.browser = .rustgd_state$prev_launch_browser)
  invisible(NULL)
}

#' Show a live URL in the rustgd web window.
#'
#' Points the rustgd webview window at a running address: a Shiny app, a local
#' dashboard, a development server, or any reachable http(s) page. Unlike an
#' htmlwidget, nothing is copied; the window loads the URL directly, so the
#' entry is only meaningful while that server is up.
#'
#' Because a stopped server leaves a dead address behind, pushing any URL first
#' clears any previous URL entry from the window, so there is at most one live
#' URL at a time. Your htmlwidget entries are left untouched. The last URL
#' stays until you close it with the window's "Clear widget" button.
#'
#' @param url A single http:// or https:// address.
#' @param title Optional label stored with the entry.
#' @return The URL, invisibly.
#' @export
rustgd_browse <- function(url, title = NULL) {
  if (!is.character(url) || length(url) != 1L || !nzchar(url)) {
    stop("rustgd_browse(): `url` must be a single non-empty string.", call. = FALSE)
  }
  if (!grepl("^https?://", url, ignore.case = TRUE)) {
    stop("rustgd_browse(): `url` must be an http:// or https:// address.", call. = FALSE)
  }
  tryCatch(
    .rustgd_push_url(url, title = title),
    error = function(e) {
      warning("rustgd: could not open URL: ", conditionMessage(e))
    }
  )
  invisible(url)
}

#' Internal: the function registered as `options(shiny.launch.browser)`. Shiny
#' calls this with the running app's URL once the server is listening. We show
#' it in the rustgd window and register a [shiny::onStop()] callback that
#' removes this app's entry when the app exits. Because `onStop` fires when
#' `runApp()` exits (including when you interrupt it with Ctrl+C), stopping the
#' app both frees the terminal and clears the now dead pane from the window, so
#' there is no separate clear step. If showing it fails, fall back to the
#' default browser, and never let an error here interrupt `runApp()`.
#' @noRd
rustgd_shiny_launch <- function(url, ...) {
  tryCatch(
    {
      descriptor <- .rustgd_push_url(url, title = "Shiny app")
      if (requireNamespace("shiny", quietly = TRUE)) {
        force(descriptor)
        tryCatch(
          shiny::onStop(function() {
            if (file.exists(descriptor)) {
              unlink(descriptor)
            }
          }),
          error = function(e) NULL
        )
      }
    },
    error = function(e) {
      warning(
        "rustgd: could not open Shiny app in rustgd window (",
        conditionMessage(e), "); opening in the default browser instead."
      )
      tryCatch(utils::browseURL(url), error = function(e2) NULL)
    }
  )
  invisible(NULL)
}

#' Internal: the function registered as `options(viewer)`. Receives the path
#' to a rendered HTML file (or a URL) from htmlwidgets. For a file, it copies
#' the widget's bundle (its `index.html` and `lib/`) into the widget directory
#' as `widget-NNNN/`, writes a `widget-NNNN.txt` descriptor next to it, and
#' makes sure a webview window is running. A URL is handed to
#' [.rustgd_push_url()] and shown in place.
#' @noRd
rustgd_view <- function(url, height = NULL, ...) {
  tryCatch(
    {
      if (grepl("^file://", url)) {
        url <- sub("^file://", "", url)
      }
      if (grepl("^https?://", url, ignore.case = TRUE)) {
        .rustgd_push_url(url)
        return(invisible(url))
      }

      widgets <- .rustgd_widgets_dir()
      dir.create(widgets, recursive = TRUE, showWarnings = FALSE)
      .rustgd_register_cleanup()

      idx <- .rustgd_next_index(widgets)
      stem <- sprintf("widget-%04d", idx)

      # Copy the widget bundle into our own directory so the gallery, deletion,
      # export, and cleanup are all local and do not depend on R's temp folder
      # surviving. The htmlwidgets viewer path produces a dedicated folder
      # containing index.html and a lib/ dir, so copying the whole folder
      # captures exactly the widget and nothing else.
      src <- dirname(normalizePath(url, mustWork = TRUE))
      entry <- basename(url)
      bundle <- file.path(widgets, stem)
      dir.create(bundle, showWarnings = FALSE)
      file.copy(list.files(src, full.names = TRUE), bundle, recursive = TRUE)

      .rustgd_write_descriptor(widgets, stem, c("kind=file", paste0("entry=", entry)))
      rustgd_ensure_webview(widgets)
    },
    error = function(e) {
      warning("rustgd: could not display widget: ", conditionMessage(e))
    }
  )
  invisible(url)
}

#' Internal: write a `kind=url` descriptor for a live address and make sure a
#' window is running. Clears any previous URL entry first (see
#' [rustgd_browse()]) so a dead server never lingers behind a live one. Throws
#' on failure; callers wrap as needed.
#' @noRd
.rustgd_push_url <- function(url, title = NULL) {
  widgets <- .rustgd_widgets_dir()
  dir.create(widgets, recursive = TRUE, showWarnings = FALSE)
  .rustgd_register_cleanup()

  .rustgd_clear_url_descriptors(widgets)

  idx <- .rustgd_next_index(widgets)
  stem <- sprintf("widget-%04d", idx)

  lines <- c("kind=url", paste0("target=", url))
  if (!is.null(title) && nzchar(title)) {
    lines <- c(lines, paste0("title=", title))
  }
  descriptor <- .rustgd_write_descriptor(widgets, stem, lines)

  rustgd_ensure_webview(widgets)
  invisible(descriptor)
}

#' Internal: remove every `kind=url` descriptor in the widget directory,
#' leaving file (htmlwidget) entries in place. URL entries have only a
#' descriptor and no bundle folder, so removing the `.txt` is enough.
#' @noRd
.rustgd_clear_url_descriptors <- function(widgets) {
  files <- list.files(
    widgets,
    pattern = "^widget-[0-9]+\\.txt$",
    full.names = TRUE
  )
  for (f in files) {
    first <- tryCatch(
      readLines(f, n = 5L, warn = FALSE),
      error = function(e) character()
    )
    if (any(grepl("^\\s*kind\\s*=\\s*url\\s*$", first, ignore.case = TRUE))) {
      unlink(f)
    }
  }
  invisible(NULL)
}

#' Internal: the next descriptor number, one past the highest existing
#' `widget-NNNN.txt`. Numbers need only be unique among current entries, so
#' reusing a freed number after a clear is fine.
#' @noRd
.rustgd_next_index <- function(widgets) {
  existing <- list.files(widgets, pattern = "^widget-[0-9]+\\.txt$")
  nums <- suppressWarnings(
    as.integer(sub("^widget-([0-9]+)\\.txt$", "\\1", existing))
  )
  nums <- nums[!is.na(nums)]
  if (length(nums) > 0) max(nums) + 1L else 1L
}

#' Internal: write a descriptor atomically. Write to a temp name the viewer
#' ignores, then rename into place so the window never reads a half-written
#' file.
#' @noRd
.rustgd_write_descriptor <- function(widgets, stem, lines) {
  descriptor <- file.path(widgets, paste0(stem, ".txt"))
  tmp <- paste0(descriptor, ".tmp")
  writeLines(lines, tmp)
  file.rename(tmp, descriptor)
  invisible(descriptor)
}

#' Internal: launch the webview binary for the widget directory if a window is
#' not already running for it. Liveness is tracked by the close marker the
#' binary writes when its window is closed: if we launched for this directory
#' and no marker is present, the window is assumed open. (A crash that skips
#' the marker is the one case this misses; re-running `use_rustgd_webview()`
#' or displaying something else recovers.)
#' @noRd
rustgd_ensure_webview <- function(widgets) {
  closed <- file.path(widgets, "viewer_closed")
  running <- isTRUE(.rustgd_state$webview_dir == widgets) && !file.exists(closed)
  if (running) {
    return(invisible(NULL))
  }

  unlink(closed)
  bin <- rustgd_webview_bin()
  system2(bin, args = widgets, wait = FALSE)
  .rustgd_state$webview_dir <- widgets
  invisible(NULL)
}

#' Internal: register a one-time finalizer that removes the widget directory
#' when R exits, so nothing is left in the temp folder. The webview binary, if
#' still open, sees its directory vanish on its next poll and exits.
.rustgd_register_cleanup <- function() {
  if (isTRUE(.rustgd_state$cleanup_registered)) {
    return(invisible(NULL))
  }
  reg.finalizer(
    .rustgd_state,
    function(e) {
      dir <- .rustgd_widgets_dir()
      if (dir.exists(dir)) {
        unlink(dir, recursive = TRUE, force = TRUE)
      }
    },
    onexit = TRUE
  )
  .rustgd_state$cleanup_registered <- TRUE
  invisible(NULL)
}

#' Internal: absolute path to the staged rustgd-webview binary.
rustgd_webview_bin <- function() {
  exe <- if (.Platform$OS.type == "windows") "rustgd-webview.exe" else "rustgd-webview"
  bin <- system.file("bin", exe, package = "rustgd")
  if (!nzchar(bin)) {
    stop(
      "rustgd-webview binary not found. Build it with ",
      "`cargo build --release --bin rustgd-webview --features webview` ",
      "and stage it into inst/bin.",
      call. = FALSE
    )
  }
  bin
}

#' Internal: per-process widget directory. Separate from the device's
#' rustgd-<pid> directory so the two lifecycles never interfere. Uses the
#' session temp directory so it works on every platform; the binary receives
#' this path as an argument.
.rustgd_widgets_dir <- function() {
  file.path(tempdir(), paste0("rustgd-widgets-", Sys.getpid()))
}
