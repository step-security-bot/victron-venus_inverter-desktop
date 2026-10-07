/** Identity supplied by the frontend and echoed only by the native client. */
export type TransportEvent<T> = T & { notification_session?: string }

let reservedSession: string | null = null
let activeSession: string | null = null

export function reserveTransportSession(): string {
  const token =
    typeof globalThis.crypto.randomUUID === 'function'
      ? globalThis.crypto.randomUUID()
      : Array.from(globalThis.crypto.getRandomValues(new Uint8Array(16)), (byte) =>
          byte.toString(16).padStart(2, '0')
        ).join('')
  reservedSession = token
  activeSession = null
  return token
}

export function isCurrentTransportSession(token: string | null): boolean {
  return token !== null && token === reservedSession
}

export function activateTransportSession(token: string) {
  if (isCurrentTransportSession(token)) activeSession = token
}

export function invalidateTransportSession(token: string | null) {
  if (!isCurrentTransportSession(token)) return
  reservedSession = null
  activeSession = null
}

export function acceptsCurrentTransportEvent(payload: unknown): boolean {
  return (
    activeSession !== null &&
    payload !== null &&
    typeof payload === 'object' &&
    'notification_session' in payload &&
    payload.notification_session === activeSession
  )
}

export function deactivateTransportSession(token: string) {
  if (isCurrentTransportSession(token)) activeSession = null
}

export function getCurrentTransportSession(): string | null {
  return activeSession
}
