import { enableAutoUnmount, mount } from '@vue/test-utils'
import { getInstanceByDom, setPlatformAPI, use } from 'echarts/core'
import { SVGRenderer } from 'echarts/renderers'
import { afterEach, assert, describe, expect, it, vi } from 'vitest'
import { nextTick } from 'vue'
import { INIT_OPTIONS_KEY } from 'vue-echarts'
import ChartPanel from '../components/ChartPanel.vue'

// Exercise the real chart integration without depending on jsdom's canvas/layout.
enableAutoUnmount(afterEach)
use([SVGRenderer])
setPlatformAPI({ measureText: (text) => ({ width: String(text).length * 6 }) })

const option = (series: { id: string; data: number[] }[]) => ({
  animation: false,
  xAxis: { type: 'category', data: ['12:00', '12:05'] },
  yAxis: { type: 'value' },
  series: series.map((item) => ({ ...item, type: 'line' })),
})

const mountChart = (chartOption: ReturnType<typeof option>) =>
  mount(ChartPanel, {
    attachTo: document.body,
    props: { chartOption },
    global: {
      provide: { [INIT_OPTIONS_KEY]: { renderer: 'svg', width: 640, height: 300 } },
    },
  })

afterEach(() => {
  vi.unstubAllGlobals()
  document.body.innerHTML = ''
})

describe('ChartPanel chart lifecycle', () => {
  it('updates readings and removes obsolete series without recreating the chart', async () => {
    vi.stubGlobal(
      'ResizeObserver',
      class {
        observe() {}
        unobserve() {}
        disconnect() {}
      }
    )
    const wrapper = mountChart(
      option([
        { id: 'solar', data: [10, 20] },
        { id: 'grid', data: [2, 3] },
      ])
    )
    await nextTick()
    const host = wrapper.get('.echarts-host').element as HTMLElement
    const chart = getInstanceByDom(host)
    assert(chart, 'the mounted chart should initialize ECharts')
    expect(host.querySelector('svg')).not.toBeNull()
    expect(chart.getOption().series).toMatchObject([
      { id: 'solar', data: [10, 20] },
      { id: 'grid', data: [2, 3] },
    ])

    await wrapper.setProps({ chartOption: option([{ id: 'solar', data: [0, -10] }]) })
    expect(getInstanceByDom(host)).toBe(chart)
    expect(chart.getOption().series).toMatchObject([{ id: 'solar', data: [0, -10] }])
    expect(chart.getOption().series).toHaveLength(1)
    wrapper.unmount()
    await nextTick()
    expect(chart.isDisposed()).toBe(true)
  })

  it('disconnects resize observation and disposes the renderer on unmount', async () => {
    const disconnect = vi.fn()
    vi.stubGlobal(
      'ResizeObserver',
      class {
        observe() {}
        unobserve() {}
        disconnect = disconnect
      }
    )
    const wrapper = mountChart(option([{ id: 'solar', data: [1, 2] }]))
    await nextTick()
    const host = wrapper.get('.echarts-host').element as HTMLElement
    const chart = getInstanceByDom(host)
    assert(chart, 'the mounted chart should initialize ECharts')
    wrapper.unmount()
    await nextTick()
    expect(disconnect).toHaveBeenCalledOnce()
    expect(chart.isDisposed()).toBe(true)
    expect(getInstanceByDom(host)).toBeUndefined()
  })
})
