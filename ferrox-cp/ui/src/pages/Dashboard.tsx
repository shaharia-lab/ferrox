import { useQuery } from '@tanstack/react-query'
import { Link } from 'react-router-dom'
import { Users, Key, ClipboardList, ExternalLink } from 'lucide-react'
import { api, type AuditEntry, type Client, type SigningKey } from '../api'
import { Card, CardBody } from '../components/ui/Card'

function StatCard({
  label,
  value,
  icon: Icon,
  to,
}: {
  label: string
  value: string | number
  icon: React.ElementType
  to: string
}) {
  return (
    <Link to={to}>
      <Card className="hover:shadow-md transition-shadow">
        <CardBody className="flex items-center gap-4">
          <div className="rounded-full bg-indigo-50 p-3">
            <Icon className="h-5 w-5 text-indigo-600" />
          </div>
          <div>
            <p className="text-sm text-gray-500">{label}</p>
            <p className="text-2xl font-semibold text-gray-900">{value}</p>
          </div>
        </CardBody>
      </Card>
    </Link>
  )
}

function fmtDate(iso: string) {
  return new Date(iso).toLocaleString()
}

export default function Dashboard() {
  const clients = useQuery<Client[]>({
    queryKey: ['clients'],
    queryFn: () => api.clients.list(1000),
  })
  const keys = useQuery<SigningKey[]>({
    queryKey: ['signing-keys'],
    queryFn: () => api.signingKeys.list(),
  })
  const audit = useQuery<AuditEntry[]>({
    queryKey: ['audit', { limit: 10 }],
    queryFn: () => api.audit.list({ limit: 10 }),
  })

  const activeClients = clients.data?.filter((c) => c.active).length ?? '—'
  const activeKeys = keys.data?.filter((k) => k.active).length ?? '—'

  return (
    <div className="space-y-6">
      <div>
        <h1 className="text-2xl font-bold text-gray-900">Dashboard</h1>
        <p className="text-sm text-gray-500 mt-1">Control plane overview</p>
      </div>

      <div className="grid grid-cols-1 sm:grid-cols-3 gap-4">
        <StatCard
          label="Active clients"
          value={activeClients}
          icon={Users}
          to="/clients"
        />
        <StatCard
          label="Active signing keys"
          value={activeKeys}
          icon={Key}
          to="/signing-keys"
        />
        <Card className="hover:shadow-md transition-shadow">
          <a
            href="http://localhost:3000"
            target="_blank"
            rel="noopener noreferrer"
          >
            <CardBody className="flex items-center gap-4">
              <div className="rounded-full bg-orange-50 p-3">
                <ExternalLink className="h-5 w-5 text-orange-600" />
              </div>
              <div>
                <p className="text-sm text-gray-500">Grafana</p>
                <p className="text-sm font-medium text-indigo-600">
                  Open dashboard ↗
                </p>
              </div>
            </CardBody>
          </a>
        </Card>
      </div>

      <Card>
        <div className="px-6 py-4 border-b border-gray-200 flex items-center gap-2">
          <ClipboardList className="h-4 w-4 text-gray-400" />
          <h2 className="font-semibold text-gray-900">Recent audit events</h2>
        </div>
        {audit.isLoading && (
          <CardBody>
            <p className="text-sm text-gray-500">Loading…</p>
          </CardBody>
        )}
        {audit.isError && (
          <CardBody>
            <p className="text-sm text-red-500">Failed to load audit events.</p>
          </CardBody>
        )}
        {audit.data && (
          <div className="divide-y divide-gray-100">
            {audit.data.length === 0 && (
              <CardBody>
                <p className="text-sm text-gray-500">No events yet.</p>
              </CardBody>
            )}
            {audit.data.map((entry) => (
              <div key={entry.id} className="px-6 py-3 flex items-center gap-3">
                <span className="inline-flex items-center rounded-full bg-gray-100 px-2 py-0.5 text-xs font-medium text-gray-700">
                  {entry.event}
                </span>
                <span className="text-sm text-gray-600 flex-1 truncate">
                  {entry.client_id ?? 'system'}
                </span>
                <span className="text-xs text-gray-400 shrink-0">
                  {fmtDate(entry.created_at)}
                </span>
              </div>
            ))}
          </div>
        )}
      </Card>
    </div>
  )
}
