import { useState } from 'react'
import { useQuery } from '@tanstack/react-query'
import { ChevronDown } from 'lucide-react'
import { api, type AuditEntry, type Client } from '../api'
import { Card } from '../components/ui/Card'
import Input from '../components/ui/Input'
import Label from '../components/ui/Label'

const EVENT_TYPES = [
  { value: '', label: 'All events' },
  { value: 'token_issued', label: 'Token issued' },
  { value: 'client_created', label: 'Client created' },
  { value: 'client_revoked', label: 'Client revoked' },
  { value: 'key_rotated', label: 'Key rotated' },
]

function fmtDate(iso: string) {
  return new Date(iso).toLocaleString()
}

function MetadataCell({ meta }: { meta: Record<string, unknown> | undefined }) {
  const [open, setOpen] = useState(false)
  if (!meta) return <span className="text-gray-400">—</span>
  const preview = JSON.stringify(meta).slice(0, 60)
  const hasMore = JSON.stringify(meta).length > 60
  return (
    <div>
      <button
        className="text-left text-xs font-mono text-gray-500 hover:text-gray-800 flex items-center gap-1 cursor-pointer"
        onClick={() => setOpen((v) => !v)}
      >
        {open ? JSON.stringify(meta, null, 2) : preview + (hasMore ? '…' : '')}
        {hasMore && (
          <ChevronDown
            className={`h-3 w-3 shrink-0 transition-transform ${open ? 'rotate-180' : ''}`}
          />
        )}
      </button>
    </div>
  )
}

export default function AuditLog() {
  const [clientId, setClientId] = useState('')
  const [event, setEvent] = useState('')
  const [since, setSince] = useState('')

  const clients = useQuery<Client[]>({
    queryKey: ['clients'],
    queryFn: () => api.clients.list(1000),
  })

  const entries = useQuery<AuditEntry[]>({
    queryKey: ['audit', { clientId, event, since }],
    queryFn: () =>
      api.audit.list({
        client_id: clientId || undefined,
        event: event || undefined,
        since: since ? new Date(since).toISOString() : undefined,
        limit: 200,
      }),
  })

  return (
    <div className="space-y-6">
      <div>
        <h1 className="text-2xl font-bold text-gray-900">Audit Log</h1>
        <p className="text-sm text-gray-500 mt-1">
          Immutable record of all control plane events
        </p>
      </div>

      {/* Filters */}
      <div className="grid grid-cols-1 sm:grid-cols-3 gap-4">
        <div className="space-y-1.5">
          <Label htmlFor="f-client">Client</Label>
          <select
            id="f-client"
            value={clientId}
            onChange={(e) => setClientId(e.target.value)}
            className="block w-full rounded-md border border-gray-300 px-3 py-2 text-sm focus:border-indigo-500 focus:outline-none focus:ring-1 focus:ring-indigo-500 bg-white"
          >
            <option value="">All clients</option>
            {clients.data?.map((c) => (
              <option key={c.id} value={c.id}>
                {c.name}
              </option>
            ))}
          </select>
        </div>
        <div className="space-y-1.5">
          <Label htmlFor="f-event">Event type</Label>
          <select
            id="f-event"
            value={event}
            onChange={(e) => setEvent(e.target.value)}
            className="block w-full rounded-md border border-gray-300 px-3 py-2 text-sm focus:border-indigo-500 focus:outline-none focus:ring-1 focus:ring-indigo-500 bg-white"
          >
            {EVENT_TYPES.map((t) => (
              <option key={t.value} value={t.value}>
                {t.label}
              </option>
            ))}
          </select>
        </div>
        <div className="space-y-1.5">
          <Label htmlFor="f-since">Since</Label>
          <Input
            id="f-since"
            type="datetime-local"
            value={since}
            onChange={(e) => setSince(e.target.value)}
          />
        </div>
      </div>

      <Card>
        {entries.isLoading && (
          <div className="px-6 py-8 text-center text-sm text-gray-500">
            Loading…
          </div>
        )}
        {entries.isError && (
          <div className="px-6 py-8 text-center text-sm text-red-500">
            Failed to load audit entries.
          </div>
        )}
        {entries.data && (
          <div className="overflow-x-auto">
            <table className="w-full text-sm">
              <thead className="bg-gray-50 text-xs text-gray-500 uppercase tracking-wide">
                <tr>
                  <th className="px-6 py-3 text-left">Time</th>
                  <th className="px-6 py-3 text-left">Client</th>
                  <th className="px-6 py-3 text-left">Event</th>
                  <th className="px-6 py-3 text-left">Metadata</th>
                </tr>
              </thead>
              <tbody className="divide-y divide-gray-100">
                {entries.data.length === 0 && (
                  <tr>
                    <td
                      colSpan={4}
                      className="px-6 py-8 text-center text-gray-500"
                    >
                      No audit entries match the current filters.
                    </td>
                  </tr>
                )}
                {entries.data.map((e) => {
                  const clientName = clients.data?.find(
                    (c) => c.id === e.client_id,
                  )?.name
                  return (
                    <tr key={e.id} className="hover:bg-gray-50">
                      <td className="px-6 py-3 text-xs text-gray-500 whitespace-nowrap">
                        {fmtDate(e.created_at)}
                      </td>
                      <td className="px-6 py-3 text-gray-600 text-xs">
                        {clientName ?? e.client_id ?? (
                          <span className="text-gray-400">system</span>
                        )}
                      </td>
                      <td className="px-6 py-3">
                        <span className="rounded-full bg-gray-100 px-2 py-0.5 text-xs font-medium text-gray-700">
                          {e.event}
                        </span>
                      </td>
                      <td className="px-6 py-3 max-w-xs">
                        <MetadataCell meta={e.metadata} />
                      </td>
                    </tr>
                  )
                })}
              </tbody>
            </table>
          </div>
        )}
      </Card>
    </div>
  )
}
