<template>
  <ErrorBoundary>
    <div class="h-screen w-screen bg-black flex flex-col select-none overflow-hidden">
      <div
        class="flex items-center justify-between px-3 py-2 bg-gradient-to-b from-black/90 to-transparent absolute top-0 left-0 right-0 z-50 pointer-events-auto"
        role="toolbar"
        aria-label="Camera window controls"
        @mousedown.left="dragOwnedWindow"
      >
        <!-- Tauri 2 drag region: title strip only so the close button stays clickable -->
        <div class="flex items-center gap-2 min-w-0 flex-1 h-full cursor-default">
          <div
            class="w-1.5 h-1.5 rounded-full bg-red-500 animate-pulse shrink-0 pointer-events-none"
          ></div>
          <span
            class="text-[11px] font-semibold text-white tracking-tight truncate pointer-events-none"
          >
            {{ cameraName }}
          </span>
        </div>
        <button
          type="button"
          class="p-1.5 rounded-full bg-white/10 text-white hover:bg-white/20 transition-colors shrink-0"
          aria-label="Close"
          @click="closeWindow"
        >
          <X :size="18" />
        </button>
      </div>

      <video
        v-if="videoUrl && mediaKind === 'video'"
        ref="videoElement"
        autoplay
        muted
        playsinline
        class="w-full h-full object-contain bg-black"
        :src="videoUrl"
        @loadedmetadata="startPlayback"
        @playing="onVideoPlaying"
        @ended="closeWindow"
        @error="onMediaError"
      >
        <track kind="captions" />
        Your browser does not support the video tag.
      </video>

      <img
        v-else-if="videoUrl && (mediaKind === 'image' || mediaKind === 'live')"
        ref="imageElement"
        class="w-full h-full object-contain bg-black"
        :src="videoUrl"
        :alt="mediaKind === 'live' ? 'Live camera' : 'Camera snapshot'"
        @load="onImageLoad"
        @error="onMediaError"
      />

      <div
        v-else
        class="flex-1 min-h-0 flex flex-col overflow-y-auto text-white/70 text-[13px] mt-12 px-4 pb-4 text-center"
      >
        <p
          :role="errorMessage ? 'alert' : undefined"
          class="my-auto shrink-0 whitespace-pre-wrap [overflow-wrap:anywhere]"
        >
          {{
            errorMessage ||
            (mediaKind === 'live' ? 'Connecting to live camera…' : 'Waiting for camera clip…')
          }}
        </p>
      </div>
    </div>
  </ErrorBoundary>
</template>

<script setup lang="ts">
import { X } from '@lucide/vue'
import { convertFileSrc, invoke } from '@tauri-apps/api/core'
import { getCurrentWindow } from '@tauri-apps/api/window'
import { nextTick, onBeforeUnmount, onMounted, ref } from 'vue'
import ErrorBoundary from '../../../components/ErrorBoundary.vue'
import { logger } from '../../../logger'
import { pluginVideoRoute } from './pluginVideoRoute'

const videoUrl = ref('')
const cameraName = ref('Camera')
const errorMessage = ref('')
const mediaKind = ref<'video' | 'image' | 'live'>('video')
const videoElement = ref<HTMLVideoElement | null>(null)
const imageElement = ref<HTMLImageElement | null>(null)
let disposed = false
let closing = false
let ownedRoute = false
let revealRequested = false
let playing = false
let imageCloseTimer: ReturnType<typeof setTimeout> | null = null
let readyTimer: ReturnType<typeof setTimeout> | null = null
let framePoll: ReturnType<typeof setInterval> | null = null
let imageReadinessContext: CanvasRenderingContext2D | null = null

function clearReadinessTimers() {
  if (readyTimer !== null) clearTimeout(readyTimer)
  if (framePoll !== null) clearInterval(framePoll)
  readyTimer = null
  framePoll = null
}

function releaseMedia() {
  // Removing a node (or updating a ref after unmount) does not explicitly end
  // its media request. Release the source while the element is still available,
  // especially for an ongoing MJPEG response and a pending video.play().
  imageElement.value?.removeAttribute('src')
  const video = videoElement.value
  if (video?.hasAttribute('src')) {
    video.pause()
    video.removeAttribute('src')
    video.load()
  }
  videoUrl.value = ''
  playing = false
  imageReadinessContext = null
}

async function revealWindow() {
  if (!ownedRoute || disposed || closing || revealRequested) return
  revealRequested = true
  clearReadinessTimers()
  // Flush a ready frame's element or the final error message before showing the
  // native window. Animation callbacks can be suspended while it is hidden.
  await nextTick()
  if (disposed || closing) return
  try {
    await invoke('reveal_plugin_video_window')
    if (!disposed && !closing && !errorMessage.value && mediaKind.value === 'image') {
      startImageCloseTimer()
    }
  } catch {
    if (disposed || closing) return
    logger.warn('Cannot reveal owned plugin video window')
    void closeWindow()
  }
}

function checkFirstFrame() {
  if (disposed || closing || errorMessage.value) return
  if (mediaKind.value === 'live') {
    // Poll too: an ongoing MJPEG response need not produce repeated load events.
    if (hasImageFrame()) void revealWindow()
  } else if (mediaKind.value === 'video') {
    const video = videoElement.value
    if (
      playing &&
      video &&
      !video.paused &&
      !video.ended &&
      video.readyState >= HTMLMediaElement.HAVE_CURRENT_DATA &&
      video.videoWidth > 0 &&
      video.videoHeight > 0
    ) {
      void revealWindow()
    }
  }
}

function hasImageFrame() {
  const image = imageElement.value
  if (!image || image.naturalWidth === 0 || image.naturalHeight === 0) return false
  imageReadinessContext ??= document.createElement('canvas').getContext('2d')
  try {
    // Dimensions alone can come from a JPEG header before any pixels arrive.
    // A pattern requires a decodable image (the first complete MJPEG part also
    // qualifies). No drawing or pixel reads, so camera CORS is not required.
    return imageReadinessContext?.createPattern(image, 'no-repeat') != null
  } catch {
    return false
  }
}

async function startPlayback() {
  const video = videoElement.value
  if (!video || disposed || closing || errorMessage.value) return
  try {
    // Explicit muted playback also starts in a hidden WebKit window, where
    // visibility-based autoplay may wait for the window to be shown.
    await video.play()
  } catch {
    if (!disposed && !closing) onMediaError()
  }
}

function onVideoPlaying() {
  playing = true
  checkFirstFrame()
}

function waitForFirstFrame() {
  readyTimer = setTimeout(() => {
    showError('Camera media did not become ready in time.', cameraName.value)
  }, 10000)
  framePoll = setInterval(checkFirstFrame, 100)
}

function setName(name?: string | null) {
  cameraName.value = name?.trim() || 'Camera'
}

function clearImageCloseTimer() {
  if (imageCloseTimer !== null) {
    clearTimeout(imageCloseTimer)
    imageCloseTimer = null
  }
}

function startImageCloseTimer() {
  // Stills have no @ended. Repeated load events must not extend their lifetime.
  if (imageCloseTimer !== null) return
  imageCloseTimer = setTimeout(() => {
    void closeWindow()
  }, 12000)
}

function onImageLoad() {
  if (disposed || closing || revealRequested || errorMessage.value) return
  if (hasImageFrame()) void revealWindow()
}

function showError(message: string, name?: string | null) {
  if (disposed || closing || errorMessage.value) return
  setName(name)
  errorMessage.value = message
  clearReadinessTimers()
  clearImageCloseTimer()
  releaseMedia()
  logger.warn('Camera clip error:', message)
  void revealWindow()
}

function onMediaError() {
  if (disposed || closing || errorMessage.value) return
  clearReadinessTimers()
  clearImageCloseTimer()
  errorMessage.value =
    mediaKind.value === 'live'
      ? 'Failed to display live camera preview.'
      : mediaKind.value === 'image'
        ? 'Failed to display camera snapshot. Local file may be missing or unsupported.'
        : 'Failed to play camera clip. Local file may be missing or unsupported.'
  logger.warn(
    mediaKind.value === 'live'
      ? 'Live camera preview failed'
      : mediaKind.value === 'image'
        ? 'Plugin image display failed'
        : 'Plugin video playback failed'
  )
  releaseMedia()
  void revealWindow()
}

async function closeWindow() {
  if (closing || disposed) return
  closing = true
  clearReadinessTimers()
  clearImageCloseTimer()
  releaseMedia()
  try {
    await invoke('close_plugin_video_window')
  } catch {
    closing = false
    logger.warn('Cannot close owned plugin video window')
  }
}

async function dragOwnedWindow(event: MouseEvent) {
  if (!(event.target instanceof Element) || event.target.closest('button')) {
    return
  }
  try {
    await invoke('drag_plugin_video_window')
  } catch {
    logger.warn('Cannot drag owned plugin video window')
  }
}

onMounted(async () => {
  let label = ''
  try {
    label = getCurrentWindow().label
  } catch {
    // Browser-only previews have no native window or plugin media authority.
  }
  const route = pluginVideoRoute(globalThis.location.search, label)
  ownedRoute = route !== null
  if (!route) showError('This video window is unavailable.')
  else if (route.failed)
    showError(
      route.mediaKind === 'live'
        ? 'Failed to open live camera preview.'
        : route.mediaKind === 'image'
          ? 'Failed to download camera snapshot.'
          : 'Failed to download camera clip.',
      route.name
    )
  else {
    setName(route.name)
    mediaKind.value = route.mediaKind
    waitForFirstFrame()
    if (route.mediaKind === 'live') {
      try {
        // The host resolves only this window's active, verified grant. Source
        // URLs never come from route parameters, snapshots, or arbitrary IDs.
        const url = await invoke<string>('get_live_preview_url')
        if (disposed || closing || errorMessage.value) return
        if (typeof url !== 'string' || !url.trim()) throw new Error('Live preview unavailable')
        const parsed = new URL(url)
        if (!['http:', 'https:'].includes(parsed.protocol) || parsed.username || parsed.password) {
          throw new Error('Live preview unavailable')
        }
        videoUrl.value = url
      } catch {
        if (!disposed) showError('Failed to open live camera preview.', route.name)
      }
    } else {
      videoUrl.value = convertFileSrc(route.id, 'plugin-media')
    }
  }
})

onBeforeUnmount(() => {
  disposed = true
  releaseMedia()
  clearImageCloseTimer()
  clearReadinessTimers()
})
</script>
