import { useParams, Link } from 'react-router-dom'
import { useQuery } from '@tanstack/react-query'
import { ArrowLeft } from 'lucide-react'
import {
  BarChart,
  Bar,
  XAxis,
  YAxis,
  CartesianGrid,
  Tooltip,
  ResponsiveContainer,
} from 'recharts'
import { api, type Client, type UsageSummary, type UsageStats, type AuditEntry } from '../api'
import { Card, CardHeader, CardBody } from '../components/ui/Card'
import Badge from '../components/ui/Badge'

function fmtDate(iso: string) {
  return new Date(iso).toLocaleString()
}

function StatBox({ label, value }: { label: string; value: UsageSummary }) {
  return (
    <div className="text-center">
      <p className="text-2xl font-semibold text-gray-900">{value.total_tokens.toLocaleString()}</p>
      <p className="text-xs text-gray-500 mt-0.5">{label}</p>
      <p className="text-xs text-gray-400 mt-0.5">
        {value.request_count.toLocaleString()} requests
      </p>
    </div>
  )
}

export default function ClientDetail() {
  const { id } = useParams<{ id: string }>()

  const client = useQuery<Client>({
    queryKey: ['client', id],
    queryFn: () => api.clients.get(id!),
    enabled: Boolean(id),
  })

  const usage = useQuery<UsageStats>({
    queryKey: ['client-usage', id],
    queryFn: () => api.clients.usage(id!),
    enabled: Boolean(id),
  })

  const audit = useQuery<AuditEntry[]>({
    queryKey: ['audit', { client_id: id }],
    queryFn: () => api.audit.list({ client_id: id, limit: 50 }),
    enabled: Boolean(id),
  })

  const usageChartData = usage.data
    ? [
        { period: 'Last 24h', tokens: usage.data.last_24h.total_tokens },
        { period: 'Last 7d', tokens: usage.data.last_7d.total_tokens },
        { period: 'Last 30d', tokens: usage.data.last_30d.total_tokens },
      ]
    : []

  if (client.isError) {
    return (
      <div className="space-y-4">
        <Link
          to="/clients"
          className="flex items-center gap-1.5 text-sm text-gray-500 hover:text-gray-700"
        >
          <ArrowLeft className="h-4 w-4" /> Back to clients
        </Link>
        <p className="text-red-500">Failed to load client.</p>
      </div>
    )
  }

  return (
    <div className="space-y-6">
      <div className="flex items-center gap-3">
        <Link
          to="/clients"
          className="flex items-center gap-1.5 text-sm text-gray-500 hover:text-gray-700"
        >
          <ArrowLeft className="h-4 w-4" /> Back
        </Link>
        <h1 className="text-2xl font-bold text-gray-900">
          {client.data?.name ?? '…'}
        </h1>
        {client.data && (
          <Badge variant={client.data.active ? 'green' : 'red'}>
            {client.data.active ? 'active' : 'revoked'}
          </Badge>
        )}
      </div>

      {/* Client details */}
      <Card>
        <CardHeader>
          <h2 className="font-semibold text-gray-900">Details</h2>
        </CardHeader>
        {client.isLoading && (
          <CardBody>
            <p className="text-sm text-gray-500">Loading…</p>
          </CardBody>
        )}
        {client.data && (
          <CardBody>
            <dl className="grid grid-cols-2 gap-x-6 gap-y-3 text-sm">
              <div>
                <dt className="text-gray-500">ID</dt>
                <dd className="font-mono text-xs mt-0.5 text-gray-700">
                  {client.data.id}
                </dd>
              </div>
              <div>
                <dt className="text-gray-500">Allowed models</dt>
                <dd className="mt-0.5 text-gray-700">
                  {client.data.allowed_models.join(', ')}
                </dd>
              </div>
              <div>
                <dt className="text-gray-500">Rate limit</dt>
                <dd className="mt-0.5 text-gray-700">
                  {client.data.rpm} RPM / burst {client.data.burst}
                </dd>
              </div>
              <div>
                <dt className="text-gray-500">Token TTL</dt>
                <dd className="mt-0.5 text-gray-700">
                  {client.data.token_ttl_seconds}s
                </dd>
              </div>
              <div>
                <dt className="text-gray-500">Created</dt>
                <dd className="mt-0.5 text-gray-700">
                  {fmtDate(client.data.created_at)}
                </dd>
              </div>
              {client.data.revoked_at && (
                <div>
                  <dt className="text-gray-500">Revoked</dt>
                  <dd className="mt-0.5 text-red-700">
                    {fmtDate(client.data.revoked_at)}
                  </dd>
                </div>
              )}
              {client.data.description && (
                <div className="col-span-2">
                  <dt className="text-gray-500">Description</dt>
                  <dd className="mt-0.5 text-gray-700">
                    {client.data.description}
                  </dd>
                </div>
              )}
            </dl>
          </CardBody>
        )}
      </Card>

      {/* Usage */}
      <Card>
        <CardHeader>
          <h2 className="font-semibold text-gray-900">Token usage</h2>
        </CardHeader>
        {usage.isLoading && (
          <CardBody>
            <p className="text-sm text-gray-500">Loading…</p>
          </CardBody>
        )}
        {usage.data && (
          <CardBody>
            <div className="flex justify-around mb-6 py-2 bg-gray-50 rounded-lg">
              <StatBox label="Last 24h" value={usage.data.last_24h} />
              <StatBox label="Last 7 days" value={usage.data.last_7d} />
              <StatBox label="Last 30 days" value={usage.data.last_30d} />
            </div>
            <ResponsiveContainer width="100%" height={160}>
              <BarChart data={usageChartData}>
                <CartesianGrid strokeDasharray="3 3" />
                <XAxis dataKey="period" tick={{ fontSize: 12 }} />
                <YAxis tick={{ fontSize: 12 }} />
                <Tooltip />
                <Bar dataKey="tokens" fill="#6366f1" radius={[4, 4, 0, 0]} />
              </BarChart>
            </ResponsiveContainer>
          </CardBody>
        )}
      </Card>

      {/* Audit log */}
      <Card>
        <CardHeader>
          <h2 className="font-semibold text-gray-900">Audit log</h2>
        </CardHeader>
        {audit.isLoading && (
          <CardBody>
            <p className="text-sm text-gray-500">Loading…</p>
          </CardBody>
        )}
        {audit.data && (
          <div className="overflow-x-auto">
            <table className="w-full text-sm">
              <thead className="bg-gray-50 text-xs text-gray-500 uppercase tracking-wide">
                <tr>
                  <th className="px-6 py-3 text-left">Time</th>
                  <th className="px-6 py-3 text-left">Event</th>
                  <th className="px-6 py-3 text-left">Metadata</th>
                </tr>
              </thead>
              <tbody className="divide-y divide-gray-100">
                {audit.data.length === 0 && (
                  <tr>
                    <td
                      colSpan={3}
                      className="px-6 py-6 text-center text-gray-500"
                    >
                      No audit entries.
                    </td>
                  </tr>
                )}
                {audit.data.map((e) => (
                  <tr key={e.id} className="hover:bg-gray-50">
                    <td className="px-6 py-3 text-gray-500 whitespace-nowrap text-xs">
                      {fmtDate(e.created_at)}
                    </td>
                    <td className="px-6 py-3">
                      <span className="rounded-full bg-gray-100 px-2 py-0.5 text-xs font-medium text-gray-700">
                        {e.event}
                      </span>
                    </td>
                    <td className="px-6 py-3 text-xs text-gray-500 font-mono max-w-xs truncate">
                      {e.metadata
                        ? JSON.stringify(e.metadata)
                        : '—'}
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
      </Card>
    </div>
  )
}
