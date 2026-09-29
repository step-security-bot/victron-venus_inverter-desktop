import { createServer, type Server } from 'node:http'
import { gzipSync } from 'node:zlib'
import { afterAll, beforeAll, beforeEach, describe, expect, it } from 'vitest'

let server: Server
let baseUrl: string
const requests: string[] = []

beforeAll(async () => {
  server = createServer((request, response) => {
    requests.push(request.url ?? '')
    response.setHeader('Access-Control-Allow-Origin', '*')
    if (request.url === '/redirect') {
      response.writeHead(302, { Location: '/payload' }).end()
    } else if (request.url === '/payload') {
      response.writeHead(200, {
        'Content-Type': 'application/json',
        'Content-Encoding': 'gzip',
      })
      response.end(gzipSync(JSON.stringify({ transport: 'jsdom', ready: true })))
    } else if (request.url === '/slow') {
      response.writeHead(200, { 'Content-Type': 'application/json' })
      response.flushHeaders()
    } else {
      response.writeHead(200, { 'Content-Type': 'application/json' }).end('{"ready":true}')
    }
  })
  await new Promise<void>((resolve) => server.listen(0, '127.0.0.1', resolve))
  const address = server.address()
  if (!address || typeof address === 'string') throw new Error('Missing fixture HTTP address')
  baseUrl = `http://127.0.0.1:${address.port}`
})

beforeEach(() => {
  requests.length = 0
})

afterAll(async () => {
  await new Promise<void>((resolve, reject) => {
    server.close((error) => (error ? reject(error) : resolve()))
    server.closeAllConnections()
  })
})

describe('jsdom network transport', () => {
  it('follows redirects and decodes compressed JSON after the native fetch dispatcher is used', async () => {
    // Node fetch and jsdom share the dispatcher symbol. Exercise both without
    // importing a second, direct Undici dependency or replacing the transport.
    const health = await fetch(`${baseUrl}/health`)
    expect(await health.json()).toEqual({ ready: true })
    const request = await new Promise<XMLHttpRequest>((resolve, reject) => {
      const xhr = new XMLHttpRequest()
      xhr.open('GET', `${baseUrl}/redirect`)
      xhr.responseType = 'json'
      xhr.timeout = 2000
      xhr.onload = () => resolve(xhr)
      xhr.onerror = () => reject(new Error('jsdom request failed'))
      xhr.ontimeout = () => reject(new Error('jsdom request timed out'))
      xhr.send()
    })
    expect(request.status).toBe(200)
    expect(request.response).toEqual({ transport: 'jsdom', ready: true })
    expect(request.responseURL).toBe(`${baseUrl}/payload`)
    expect(requests).toEqual(['/health', '/redirect', '/payload'])
  })

  it('aborts an in-flight jsdom request and closes its HTTP connection', async () => {
    const received = new Promise<void>((resolve) => server.once('request', () => resolve()))
    const closed = new Promise<void>((resolve) => {
      server.once('request', (_request, response) => response.once('close', () => resolve()))
    })
    const xhr = new XMLHttpRequest()
    const aborted = new Promise<void>((resolve, reject) => {
      xhr.onabort = () => resolve()
      xhr.onload = () => reject(new Error('Aborted request completed'))
      xhr.onerror = () => reject(new Error('jsdom request failed before abort'))
      xhr.ontimeout = () => reject(new Error('jsdom request timed out before abort'))
    })
    xhr.open('GET', `${baseUrl}/slow`)
    xhr.timeout = 2000
    xhr.send()
    await received
    xhr.abort()
    await Promise.all([aborted, closed])
    expect(xhr.readyState).toBe(XMLHttpRequest.UNSENT)
    expect(xhr.status).toBe(0)
  })
})
