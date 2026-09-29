import { flushPromises, mount, type VueWrapper } from '@vue/test-utils'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import CameraVideo from './PluginMedia.vue'

const native = vi.hoisted(() => ({
  invoke: vi.fn(),
  convertFileSrc: vi.fn(),
  getCurrentWindow: vi.fn(),
  close: vi.fn(),
  warn: vi.fn(),
  error: vi.fn(),
  createPattern: vi.fn(),
}))

vi.mock('@tauri-apps/api/core', () => ({
  invoke: native.invoke,
  convertFileSrc: native.convertFileSrc,
}))
vi.mock('@tauri-apps/api/window', () => ({ getCurrentWindow: native.getCurrentWindow }))
vi.mock('../../../logger', () => ({ logger: { warn: native.warn, error: native.error } }))

const id = 'acdb88b6-453a-4f88-8d52-5b902441fe4c'
const owner = `plugin-video-${id}`
const initialLocation = globalThis.location.href
const mounted: VueWrapper[] = []

function decodedImage(wrapper: VueWrapper) {
  Object.defineProperties(wrapper.get('img').element, {
    naturalWidth: { configurable: true, value: 640 },
    naturalHeight: { configurable: true, value: 360 },
  })
  return wrapper.get('img')
}

async function player(params: Record<string, string>, label = owner) {
  globalThis.history.replaceState({}, '', `/camera-video?${new URLSearchParams(params)}`)
  native.getCurrentWindow.mockReturnValue({ label, close: native.close })
  const wrapper = mount(CameraVideo)
  mounted.push(wrapper)
  await flushPromises()
  return wrapper
}

beforeEach(() => {
  vi.spyOn(HTMLMediaElement.prototype, 'pause').mockImplementation(() => {})
  vi.spyOn(HTMLMediaElement.prototype, 'load').mockImplementation(() => {})
  native.createPattern.mockReset().mockReturnValue({})
  vi.spyOn(HTMLCanvasElement.prototype, 'getContext').mockReturnValue({
    createPattern: native.createPattern,
  } as unknown as CanvasRenderingContext2D)
  native.invoke.mockReset().mockResolvedValue(undefined)
  native.convertFileSrc.mockReset().mockImplementation((path: string, scheme?: string) => {
    return `${scheme || 'asset'}://localhost/${path}`
  })
  native.getCurrentWindow.mockReset()
  native.close.mockReset().mockResolvedValue(undefined)
  native.warn.mockReset()
  native.error.mockReset()
})

afterEach(() => {
  for (const wrapper of mounted.splice(0)) wrapper.unmount()
  vi.useRealTimers()
  vi.restoreAllMocks()
  globalThis.history.replaceState({}, '', initialLocation)
})

describe('native-owned plugin video player', () => {
  it('keeps a clip hidden until playback has started with a decoded frame', async () => {
    vi.useFakeTimers()
    const wrapper = await player({ pluginMedia: id })
    const video = wrapper.get('video')
    const play = vi.spyOn(video.element, 'play').mockResolvedValue()
    await video.trigger('loadedmetadata')
    expect(play).toHaveBeenCalledOnce()
    await video.trigger('loadeddata')
    await video.trigger('canplay')
    await vi.advanceTimersByTimeAsync(500)
    expect(native.invoke).not.toHaveBeenCalled()
    Object.defineProperties(video.element, {
      videoWidth: { value: 640 },
      videoHeight: { value: 360 },
      readyState: { value: HTMLMediaElement.HAVE_CURRENT_DATA },
      paused: { value: false },
    })
    await vi.advanceTimersByTimeAsync(100)
    expect(native.invoke).not.toHaveBeenCalled()
    await video.trigger('playing')
    await flushPromises()
    expect(native.invoke).toHaveBeenCalledExactlyOnceWith('reveal_plugin_video_window')
    await video.trigger('playing')
    await vi.advanceTimersByTimeAsync(15000)
    expect(native.invoke).toHaveBeenCalledTimes(1)
    play.mockRestore()
  })

  it('reveals ongoing MJPEG on its first decoded frame without waiting for load', async () => {
    vi.useFakeTimers()
    native.invoke.mockResolvedValue('https://camera.invalid/live')
    const wrapper = await player(
      { pluginMedia: id, pluginMediaKind: 'live' },
      `plugin-preview-${id}`
    )
    await vi.advanceTimersByTimeAsync(500)
    expect(native.invoke).toHaveBeenCalledExactlyOnceWith('get_live_preview_url')
    decodedImage(wrapper)
    expect(wrapper.get('img').element.complete).toBe(false)
    native.createPattern.mockReturnValue(null)
    await vi.advanceTimersByTimeAsync(100)
    expect(native.invoke).toHaveBeenCalledExactlyOnceWith('get_live_preview_url')
    native.createPattern.mockReturnValue({})
    await vi.advanceTimersByTimeAsync(100)
    expect(native.invoke).toHaveBeenLastCalledWith('reveal_plugin_video_window')
    native.createPattern.mockClear()
    await wrapper.get('img').trigger('load')
    await wrapper.get('img').trigger('load')
    await vi.advanceTimersByTimeAsync(15000)
    expect(native.createPattern).not.toHaveBeenCalled()
    expect(native.invoke).toHaveBeenCalledTimes(2)
    expect(native.warn).not.toHaveBeenCalled()
  })

  it('shows a rendered timeout and ignores a late live URL', async () => {
    vi.useFakeTimers()
    let resolveUrl: (url: string) => void = () => {}
    native.invoke.mockImplementation((command) =>
      command === 'get_live_preview_url'
        ? new Promise<string>((resolve) => {
            resolveUrl = resolve
          })
        : Promise.resolve()
    )
    const wrapper = await player(
      { pluginMedia: id, pluginMediaKind: 'live' },
      `plugin-preview-${id}`
    )
    await vi.advanceTimersByTimeAsync(9999)
    expect(native.invoke).toHaveBeenCalledTimes(1)
    await vi.advanceTimersByTimeAsync(1)
    expect(wrapper.get('[role="alert"]').text()).toBe('Camera media did not become ready in time.')
    expect(native.invoke).toHaveBeenLastCalledWith('reveal_plugin_video_window')
    resolveUrl('https://private.invalid/live?secret=private')
    await flushPromises()
    expect(wrapper.find('img').exists()).toBe(false)
    expect(wrapper.html()).not.toContain('private')
    expect(native.invoke).toHaveBeenCalledTimes(2)
  })

  it.each(['timeout', 'close', 'unmount'])(
    'ignores a late live URL rejection after %s',
    async (action) => {
      vi.useFakeTimers()
      let rejectUrl: (reason: Error) => void = () => {}
      native.invoke.mockImplementation((command) =>
        command === 'get_live_preview_url'
          ? new Promise<string>((_, reject) => {
              rejectUrl = reject
            })
          : Promise.resolve()
      )
      const wrapper = await player(
        { pluginMedia: id, pluginMediaKind: 'live' },
        `plugin-preview-${id}`
      )
      if (action === 'timeout') await vi.advanceTimersByTimeAsync(10000)
      else if (action === 'close') await wrapper.get('button[aria-label="Close"]').trigger('click')
      else {
        wrapper.unmount()
        mounted.splice(mounted.indexOf(wrapper), 1)
      }
      native.invoke.mockClear()
      native.warn.mockClear()
      rejectUrl(new Error('late private URL resolution failure'))
      await flushPromises()
      expect(native.warn).not.toHaveBeenCalled()
      expect(native.invoke).not.toHaveBeenCalled()
      if (action === 'timeout') {
        expect(wrapper.get('[role="alert"]').text()).toBe(
          'Camera media did not become ready in time.'
        )
      }
    }
  )

  it.each(['timeout', 'error', 'unmount'])(
    'releases an ongoing live image request on %s',
    async (action) => {
      vi.useFakeTimers()
      native.invoke.mockResolvedValue('https://camera.invalid/live?token=private')
      const wrapper = await player(
        { pluginMedia: id, pluginMediaKind: 'live' },
        `plugin-preview-${id}`
      )
      // Keep the actual node: detaching it must not leave the remote source alive.
      const image = wrapper.get('img')
      if (action === 'timeout') await vi.advanceTimersByTimeAsync(10000)
      else if (action === 'error') await image.trigger('error')
      else {
        wrapper.unmount()
        mounted.splice(mounted.indexOf(wrapper), 1)
      }
      expect(image.element.hasAttribute('src')).toBe(false)
      expect(vi.getTimerCount()).toBe(0)
      native.warn.mockClear()
      native.invoke.mockClear()
      await image.trigger('error')
      await image.trigger('load')
      expect(native.warn).not.toHaveBeenCalled()
      expect(native.invoke).not.toHaveBeenCalled()
    }
  )

  it.each(['timeout', 'error', 'unmount'])(
    'stops local video and ignores pending playback rejection on %s',
    async (action) => {
      vi.useFakeTimers()
      const wrapper = await player({ pluginMedia: id })
      const video = wrapper.get('video')
      let rejectPlay: (reason: Error) => void = () => {}
      vi.spyOn(video.element, 'play').mockImplementation(
        () =>
          new Promise<void>((_, reject) => {
            rejectPlay = reject
          })
      )
      await video.trigger('loadedmetadata')
      if (action === 'timeout') await vi.advanceTimersByTimeAsync(10000)
      else if (action === 'error') await video.trigger('error')
      else {
        wrapper.unmount()
        mounted.splice(mounted.indexOf(wrapper), 1)
      }
      expect(video.element.hasAttribute('src')).toBe(false)
      expect(video.element.pause).toHaveBeenCalledOnce()
      expect(video.element.load).toHaveBeenCalledOnce()
      expect(vi.getTimerCount()).toBe(0)
      native.warn.mockClear()
      native.invoke.mockClear()
      rejectPlay(new Error('aborted by media cleanup'))
      await flushPromises()
      expect(native.warn).not.toHaveBeenCalled()
      expect(native.invoke).not.toHaveBeenCalled()
      if (action === 'timeout') {
        expect(wrapper.get('[role="alert"]').text()).toBe(
          'Camera media did not become ready in time.'
        )
      }
    }
  )

  it.each(['close', 'unmount'])('never reveals a pending frame after %s', async (action) => {
    vi.useFakeTimers()
    const wrapper = await player({ pluginMedia: id, pluginMediaKind: 'image' })
    const image = decodedImage(wrapper)
    if (action === 'close') await wrapper.get('button[aria-label="Close"]').trigger('click')
    else {
      wrapper.unmount()
      mounted.splice(mounted.indexOf(wrapper), 1)
    }
    native.invoke.mockClear()
    await image.trigger('load')
    await vi.advanceTimersByTimeAsync(30000)
    expect(native.invoke).not.toHaveBeenCalled()
  })

  it('closes through the owner if native reveal is rejected', async () => {
    const wrapper = await player({ pluginMedia: id, pluginMediaKind: 'image' })
    native.invoke.mockImplementation((command) =>
      command === 'reveal_plugin_video_window'
        ? Promise.reject(new Error('private reason'))
        : Promise.resolve()
    )
    await decodedImage(wrapper).trigger('load')
    await flushPromises()
    expect(native.invoke.mock.calls).toEqual([
      ['reveal_plugin_video_window'],
      ['close_plugin_video_window'],
    ])
    expect(native.warn).toHaveBeenCalledExactlyOnceWith('Cannot reveal owned plugin video window')
    expect(wrapper.html()).not.toContain('private reason')
  })

  it.each(['close', 'unmount'])(
    'ignores a late native reveal rejection after %s',
    async (action) => {
      let rejectReveal: (reason: Error) => void = () => {}
      native.invoke.mockImplementation((command) =>
        command === 'reveal_plugin_video_window'
          ? new Promise<void>((_, reject) => {
              rejectReveal = reject
            })
          : Promise.resolve()
      )
      const wrapper = await player({ pluginMedia: id, pluginMediaKind: 'image' })
      await decodedImage(wrapper).trigger('load')
      await flushPromises()
      if (action === 'close') await wrapper.get('button[aria-label="Close"]').trigger('click')
      else {
        wrapper.unmount()
        mounted.splice(mounted.indexOf(wrapper), 1)
      }
      native.invoke.mockClear()
      rejectReveal(new Error('native owner already destroyed'))
      await flushPromises()
      expect(native.warn).not.toHaveBeenCalled()
      expect(native.invoke).not.toHaveBeenCalled()
    }
  )

  it('loads a live stream only through its zero-argument owner command and leaves lifetime native', async () => {
    vi.useFakeTimers({ toFake: ['setTimeout', 'clearTimeout'] })
    native.invoke.mockResolvedValue('https://camera.invalid/api/front?token=private')
    const wrapper = await player(
      { pluginMedia: id, pluginMediaKind: 'live', videoUrl: 'https://ignored.invalid' },
      `plugin-preview-${id}`
    )
    expect(native.invoke).toHaveBeenCalledExactlyOnceWith('get_live_preview_url')
    expect(native.convertFileSrc).not.toHaveBeenCalled()
    expect(wrapper.get('img').attributes('src')).toBe(
      'https://camera.invalid/api/front?token=private'
    )
    expect(wrapper.get('img').attributes('alt')).toBe('Live camera')
    expect(wrapper.find('iframe').exists()).toBe(false)
    expect(wrapper.find('video').exists()).toBe(false)
    await decodedImage(wrapper).trigger('load')
    await flushPromises()
    await vi.advanceTimersByTimeAsync(12000)
    await decodedImage(wrapper).trigger('load')
    await flushPromises()
    await vi.advanceTimersByTimeAsync(15000)
    expect(native.invoke).toHaveBeenCalledTimes(2)
    expect(native.invoke).toHaveBeenCalledWith('reveal_plugin_video_window')
    await wrapper.get('button[aria-label="Close"]').trigger('click')
    expect(native.invoke).toHaveBeenLastCalledWith('close_plugin_video_window')
    expect(native.warn).not.toHaveBeenCalled()
  })

  it('does not attach a late live response after the viewer unmounts', async () => {
    let resolve: (value: string) => void = () => {
      throw new Error('Live request was not initialized')
    }
    native.invoke.mockImplementation(
      () =>
        new Promise<string>((complete) => {
          resolve = complete
        })
    )
    const wrapper = await player(
      { pluginMedia: id, pluginMediaKind: 'live' },
      `plugin-preview-${id}`
    )
    const element = wrapper.element
    wrapper.unmount()
    mounted.splice(mounted.indexOf(wrapper), 1)
    resolve('https://private.invalid/frame?token=secret')
    await flushPromises()
    expect(element.querySelector('img')).toBeNull()
    expect(native.convertFileSrc).not.toHaveBeenCalled()
    expect(native.warn).not.toHaveBeenCalled()
  })

  it.each(['javascript:alert(1)', 'https://user:password@camera.invalid', '', undefined])(
    'rejects an invalid live descriptor without logging its value',
    async (value) => {
      native.invoke.mockResolvedValue(value)
      const wrapper = await player(
        { pluginMedia: id, pluginMediaKind: 'live' },
        `plugin-preview-${id}`
      )
      expect(wrapper.find('img').exists()).toBe(false)
      expect(wrapper.text()).toContain('Failed to open live camera preview.')
      expect(native.warn).toHaveBeenCalledExactlyOnceWith(
        'Camera clip error:',
        'Failed to open live camera preview.'
      )
    }
  )

  it('clears a failed live stream without exposing its URL or retaining a still timer', async () => {
    vi.useFakeTimers({ toFake: ['setTimeout', 'clearTimeout'] })
    native.invoke.mockResolvedValue('https://camera.invalid/frame?token=private')
    const wrapper = await player(
      { pluginMedia: id, pluginMediaKind: 'live' },
      `plugin-preview-${id}`
    )
    await decodedImage(wrapper).trigger('load')
    await flushPromises()
    await wrapper.get('img').trigger('error')
    expect(wrapper.find('img').exists()).toBe(false)
    expect(wrapper.html()).not.toContain('private')
    expect(native.warn).toHaveBeenCalledExactlyOnceWith('Live camera preview failed')
    await vi.advanceTimersByTimeAsync(15000)
    expect(native.invoke).toHaveBeenCalledTimes(2)
  })

  it('resolves only the opaque handle and plays inline and muted despite legacy URL fields', async () => {
    const wrapper = await player({
      pluginMedia: id,
      name: '<img src=x onerror=alert(1)>',
      localPath: '/private/unrelated-file.mp4',
      videoUrl: 'https://unrelated.invalid/private.mp4',
      url: 'https://unrelated.invalid/other.mp4',
      media: 'image',
    })
    expect(native.convertFileSrc).toHaveBeenCalledExactlyOnceWith(id, 'plugin-media')
    const video = wrapper.get('video')
    expect(video.attributes('src')).toBe(`plugin-media://localhost/${id}`)
    expect(video.element.autoplay).toBe(true)
    expect(video.element.muted).toBe(true)
    expect(video.element.playsInline).toBe(true)
    expect(wrapper.find('img').exists()).toBe(false)
    expect(wrapper.text()).toContain('<img src=x onerror=alert(1)>')
    expect(wrapper.html()).not.toContain('/private/unrelated-file.mp4')
    expect(wrapper.html()).not.toContain('unrelated.invalid')
    expect(native.invoke).not.toHaveBeenCalled()
  })

  it.each([
    ['main', id],
    ['config', id],
    ['camera-video-legacy', id],
    ['plugin-video-other', id],
    [owner, ''],
    [owner, '../other'],
    [owner, id.toUpperCase()],
  ])(
    'rejects owner %s with identifier %s without falling back to a local asset',
    async (label, mediaId) => {
      const wrapper = await player(
        {
          pluginMedia: mediaId,
          localPath: '/private/unrelated-file.mp4',
          videoUrl: 'https://unrelated.invalid/private.mp4',
        },
        label
      )
      expect(wrapper.text()).toContain('This video window is unavailable.')
      expect(wrapper.find('video').exists()).toBe(false)
      expect(wrapper.find('img').exists()).toBe(false)
      expect(native.convertFileSrc).not.toHaveBeenCalled()
      expect(native.invoke).not.toHaveBeenCalled()
      expect(wrapper.html()).not.toContain('unrelated.invalid')
      expect(wrapper.html()).not.toContain('/private/unrelated-file.mp4')
    }
  )

  it('requires an identifier even when the native window has a plugin owner label', async () => {
    const wrapper = await player({ localPath: '/private/legacy.mp4', media: 'image' })
    expect(wrapper.text()).toContain('This video window is unavailable.')
    expect(wrapper.find('video').exists()).toBe(false)
    expect(wrapper.find('img').exists()).toBe(false)
    expect(native.convertFileSrc).not.toHaveBeenCalled()
  })

  it.each(['ended', 'click'])('uses only the owned native close command on %s', async (event) => {
    const wrapper = await player({ pluginMedia: id })
    const target =
      event === 'ended' ? wrapper.get('video') : wrapper.get('button[aria-label="Close"]')
    await target.trigger(event)
    await flushPromises()
    expect(native.invoke).toHaveBeenCalledExactlyOnceWith('close_plugin_video_window')
    expect(native.close).not.toHaveBeenCalled()
  })

  it('does not fall back to generic window close when the owned close command fails', async () => {
    native.invoke.mockRejectedValue(new Error('private native error https://private.invalid'))
    const wrapper = await player({ pluginMedia: id })
    await wrapper.get('button[aria-label="Close"]').trigger('click')
    await flushPromises()
    expect(native.invoke).toHaveBeenCalledExactlyOnceWith('close_plugin_video_window')
    expect(native.close).not.toHaveBeenCalled()
    expect(native.warn).toHaveBeenCalledExactlyOnceWith('Cannot close owned plugin video window')
    expect(wrapper.html()).not.toContain('private.invalid')
    native.invoke.mockResolvedValue(undefined)
    await wrapper.get('button[aria-label="Close"]').trigger('click')
    expect(native.invoke).toHaveBeenCalledTimes(2)
    expect(native.invoke).toHaveBeenLastCalledWith('close_plugin_video_window')
  })

  it.each(['live', 'video'])(
    'releases %s before the native close command settles',
    async (kind) => {
      let resolveClose: () => void = () => {}
      native.invoke.mockImplementation((command) =>
        command === 'get_live_preview_url'
          ? Promise.resolve('https://camera.invalid/live')
          : new Promise<void>((resolve) => {
              resolveClose = resolve
            })
      )
      const wrapper = await player(
        { pluginMedia: id, pluginMediaKind: kind },
        kind === 'live' ? `plugin-preview-${id}` : owner
      )
      const media = wrapper.get(kind === 'live' ? 'img' : 'video').element
      await wrapper.get('button[aria-label="Close"]').trigger('click')
      expect(media.hasAttribute('src')).toBe(false)
      if (media instanceof HTMLVideoElement) {
        expect(media.pause).toHaveBeenCalledOnce()
        expect(media.load).toHaveBeenCalledOnce()
      }
      expect(native.invoke).toHaveBeenLastCalledWith('close_plugin_video_window')
      resolveClose()
      await flushPromises()
      expect(native.warn).not.toHaveBeenCalled()
    }
  )

  it('uses only the owned drag command for the title strip and no generic drag region', async () => {
    const wrapper = await player({ pluginMedia: id, name: 'Front' })
    expect(wrapper.find('[data-tauri-drag-region]').exists()).toBe(false)
    const titleStrip = wrapper.get('span').element.parentElement
    expect(titleStrip).not.toBeNull()
    titleStrip?.dispatchEvent(new MouseEvent('mousedown', { button: 0, bubbles: true }))
    await flushPromises()
    expect(native.invoke).toHaveBeenCalledExactlyOnceWith('drag_plugin_video_window')
    expect(native.close).not.toHaveBeenCalled()
    native.invoke.mockClear()
    titleStrip?.dispatchEvent(new MouseEvent('mousedown', { button: 2, bubbles: true }))
    await flushPromises()
    expect(native.invoke).not.toHaveBeenCalled()
    await wrapper.get('button[aria-label="Close"]').trigger('mousedown', { button: 0 })
    await flushPromises()
    expect(native.invoke).not.toHaveBeenCalled()
  })

  it('displays generic download failures without resolving media or exposing the source', async () => {
    const source = 'https://private.invalid/clip.mp4?token=private-token'
    const wrapper = await player({
      pluginMedia: id,
      name: 'Front',
      error: source,
      localPath: '/private/unrelated.mp4',
      videoUrl: source,
    })
    expect(wrapper.text()).toContain('Failed to download camera clip.')
    expect(wrapper.find('video').exists()).toBe(false)
    expect(wrapper.find('img').exists()).toBe(false)
    expect(native.convertFileSrc).not.toHaveBeenCalled()
    expect(native.invoke).toHaveBeenCalledExactlyOnceWith('reveal_plugin_video_window')
    expect(wrapper.html()).not.toContain('private.invalid')
    expect(wrapper.html()).not.toContain('private-token')
    expect(native.warn).toHaveBeenCalledExactlyOnceWith(
      'Camera clip error:',
      'Failed to download camera clip.'
    )
  })

  it('clears a failed local playback without logging the opaque media URL', async () => {
    const wrapper = await player({ pluginMedia: id })
    await wrapper.get('video').trigger('error')
    await flushPromises()
    expect(wrapper.find('video').exists()).toBe(false)
    expect(wrapper.text()).toContain('Failed to play camera clip.')
    expect(native.convertFileSrc).toHaveBeenCalledTimes(1)
    expect(native.invoke).toHaveBeenCalledExactlyOnceWith('reveal_plugin_video_window')
    expect(native.warn).toHaveBeenCalledExactlyOnceWith('Plugin video playback failed')
  })

  it('displays an owned still for twelve seconds after load and closes through its native owner', async () => {
    vi.useFakeTimers({ toFake: ['setTimeout', 'clearTimeout'] })
    const wrapper = await player({ pluginMedia: id, pluginMediaKind: 'image', name: 'Front' })
    expect(native.convertFileSrc).toHaveBeenCalledExactlyOnceWith(id, 'plugin-media')
    expect(wrapper.find('video').exists()).toBe(false)
    expect(wrapper.get('img').attributes('src')).toBe(`plugin-media://localhost/${id}`)
    await vi.advanceTimersByTimeAsync(500)
    expect(native.invoke).not.toHaveBeenCalled()
    await decodedImage(wrapper).trigger('load')
    await flushPromises()
    await vi.advanceTimersByTimeAsync(6000)
    await decodedImage(wrapper).trigger('load')
    await flushPromises()
    await vi.advanceTimersByTimeAsync(5999)
    expect(native.invoke).toHaveBeenCalledExactlyOnceWith('reveal_plugin_video_window')
    await vi.advanceTimersByTimeAsync(1)
    expect(native.invoke).toHaveBeenCalledTimes(2)
    expect(native.invoke).toHaveBeenLastCalledWith('close_plugin_video_window')
    expect(native.close).not.toHaveBeenCalled()
  })

  it.each(['close', 'unmount', 'error'])(
    'clears the owned still timer on %s without a delayed second close',
    async (action) => {
      vi.useFakeTimers({ toFake: ['setTimeout', 'clearTimeout'] })
      const wrapper = await player({ pluginMedia: id, pluginMediaKind: 'image' })
      await decodedImage(wrapper).trigger('load')
      await flushPromises()
      expect(native.invoke).toHaveBeenCalledExactlyOnceWith('reveal_plugin_video_window')
      native.invoke.mockClear()
      if (action === 'close') {
        await wrapper.get('button[aria-label="Close"]').trigger('click')
        expect(native.invoke).toHaveBeenCalledExactlyOnceWith('close_plugin_video_window')
        native.invoke.mockClear()
      } else if (action === 'unmount') {
        wrapper.unmount()
        mounted.splice(mounted.indexOf(wrapper), 1)
      } else {
        await wrapper.get('img').trigger('error')
        expect(wrapper.find('img').exists()).toBe(false)
        expect(wrapper.text()).toContain('Failed to display camera snapshot.')
        expect(native.warn).toHaveBeenCalledExactlyOnceWith('Plugin image display failed')
      }
      await vi.advanceTimersByTimeAsync(12000)
      expect(native.invoke).not.toHaveBeenCalled()
      expect(native.close).not.toHaveBeenCalled()
    }
  )

  it('rejects unknown media selectors without resolving or rendering content', async () => {
    const wrapper = await player({ pluginMedia: id, pluginMediaKind: 'svg' })
    expect(wrapper.text()).toContain('This video window is unavailable.')
    expect(wrapper.find('video').exists()).toBe(false)
    expect(wrapper.find('img').exists()).toBe(false)
    expect(native.convertFileSrc).not.toHaveBeenCalled()
  })

  it('renders a generic image download failure without resolving an owned handle', async () => {
    const wrapper = await player({ pluginMedia: id, pluginMediaKind: 'image', error: 'download' })
    expect(wrapper.text()).toContain('Failed to download camera snapshot.')
    expect(wrapper.find('video').exists()).toBe(false)
    expect(wrapper.find('img').exists()).toBe(false)
    expect(native.convertFileSrc).not.toHaveBeenCalled()
    expect(native.invoke).toHaveBeenCalledExactlyOnceWith('reveal_plugin_video_window')
  })

  it('rejects legacy local file routes after provider extraction', async () => {
    const wrapper = await player(
      { localPath: '/private/legacy.jpg', media: 'image' },
      'camera-video-legacy'
    )
    expect(wrapper.text()).toContain('This video window is unavailable.')
    expect(wrapper.find('img, video').exists()).toBe(false)
    expect(native.convertFileSrc).not.toHaveBeenCalled()
    expect(native.invoke).not.toHaveBeenCalled()
  })
})
