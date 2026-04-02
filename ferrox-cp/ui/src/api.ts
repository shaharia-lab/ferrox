const STORAGE_KEY = 'ferrox_admin_key'

export function getAdminKey(): string | null {
  return localStorage.getItem(STORAGE_KEY)
}

export function setAdminKey(key: string): void {
  localStorage.setItem(STORAGE_KEY, key)
}

export function clearAdminKey(): void {
  localStorage.removeItem(STORAGE_KEY)
}

function authHeaders(): Record<string, string> {
  const key = getAdminKey()
  return key ? { Authorization: `Bearer ${key}` } : {}
}

async function request<T>(path: string, init: RequestInit = {}): Promise<T> {
  const res = await fetch(path, {
    ...init,
    headers: {
      'Content-Type': 'application/json',
      ...authHeaders(),
      ...(init.headers as Record<string, string> | undefined),
    },
  })
  if (!res.ok) {
    const body = await res.text()
    throw new ApiError(res.status, body)
  }
  if (res.status === 204) return undefined as T
  return res.json() as Promise<T>
}

export class ApiError extends Error {
  constructor(public status: number, message: string) {
    super(message)
  }
}

// ── Types ─────────────────────────────────────────────────────────────────────

export interface Client {
  id: string
  name: string
  description?: string
  allowed_models: string[]
  rpm: number
  burst: number
  token_ttl_seconds: number
  active: boolean
  created_at: string
  revoked_at?: string
}

export interface CreateClientRequest {
  name: string
  description?: string
  allowed_models: string[]
  rpm: number
  burst: number
  token_ttl_seconds: number
}

export interface CreateClientResponse extends Client {
  api_key: string
}

export interface UsageStats {
  last_24h: number
  last_7d: number
  last_30d: number
}

export interface SigningKey {
  kid: string
  algorithm: string
  active: boolean
  created_at: string
  retired_at?: string
}

export interface AuditEntry {
  id: number
  client_id?: string
  event: string
  metadata?: Record<string, unknown>
  created_at: string
}

// ── API calls ─────────────────────────────────────────────────────────────────

export const api = {
  clients: {
    list: (limit = 100, offset = 0) =>
      request<Client[]>(`/api/clients?limit=${limit}&offset=${offset}`),
    get: (id: string) => request<Client>(`/api/clients/${id}`),
    create: (body: CreateClientRequest) =>
      request<CreateClientResponse>('/api/clients', {
        method: 'POST',
        body: JSON.stringify(body),
      }),
    revoke: (id: string) =>
      request<void>(`/api/clients/${id}`, { method: 'DELETE' }),
    usage: (id: string) =>
      request<UsageStats>(`/api/clients/${id}/usage`),
  },
  signingKeys: {
    list: () => request<SigningKey[]>('/api/signing-keys'),
    rotate: () =>
      request<SigningKey>('/api/signing-keys/rotate', { method: 'POST' }),
  },
  audit: {
    list: (params: {
      client_id?: string
      event?: string
      since?: string
      limit?: number
      offset?: number
    }) => {
      const q = new URLSearchParams()
      if (params.client_id) q.set('client_id', params.client_id)
      if (params.event) q.set('event', params.event)
      if (params.since) q.set('since', params.since)
      if (params.limit != null) q.set('limit', String(params.limit))
      if (params.offset != null) q.set('offset', String(params.offset))
      return request<AuditEntry[]>(`/api/audit?${q}`)
    },
  },
}
