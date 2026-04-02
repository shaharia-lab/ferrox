import { useState } from 'react'
import { useQuery, useMutation, useQueryClient } from '@tanstack/react-query'
import { Link } from 'react-router-dom'
import { Plus, Trash2 } from 'lucide-react'
import {
  api,
  type Client,
  type CreateClientRequest,
  type CreateClientResponse,
} from '../api'
import { Card } from '../components/ui/Card'
import Button from '../components/ui/Button'
import Badge from '../components/ui/Badge'
import Dialog from '../components/ui/Dialog'
import Input from '../components/ui/Input'
import Label from '../components/ui/Label'

function fmtDate(iso: string) {
  return new Date(iso).toLocaleDateString()
}

function CreateClientForm({
  onCreated,
}: {
  onCreated: (res: CreateClientResponse) => void
}) {
  const [form, setForm] = useState<CreateClientRequest>({
    name: '',
    description: '',
    allowed_models: ['*'],
    rpm: 60,
    burst: 10,
    token_ttl_seconds: 900,
  })
  const [modelsInput, setModelsInput] = useState('*')
  const [error, setError] = useState('')

  const mut = useMutation({
    mutationFn: api.clients.create,
    onSuccess: onCreated,
    onError: (e: Error) => setError(e.message),
  })

  function submit(e: React.FormEvent) {
    e.preventDefault()
    setError('')
    const models = modelsInput
      .split(',')
      .map((s) => s.trim())
      .filter(Boolean)
    mut.mutate({ ...form, allowed_models: models })
  }

  function field(key: keyof CreateClientRequest) {
    return (e: React.ChangeEvent<HTMLInputElement>) =>
      setForm((f) => ({
        ...f,
        [key]: e.target.type === 'number' ? Number(e.target.value) : e.target.value,
      }))
  }

  return (
    <form onSubmit={submit} className="space-y-4">
      <div className="space-y-1.5">
        <Label htmlFor="c-name">Name *</Label>
        <Input
          id="c-name"
          value={form.name}
          onChange={field('name')}
          placeholder="my-service"
          required
        />
      </div>
      <div className="space-y-1.5">
        <Label htmlFor="c-desc">Description</Label>
        <Input
          id="c-desc"
          value={form.description ?? ''}
          onChange={field('description')}
          placeholder="Optional"
        />
      </div>
      <div className="space-y-1.5">
        <Label htmlFor="c-models">Allowed models (comma-separated) *</Label>
        <Input
          id="c-models"
          value={modelsInput}
          onChange={(e) => setModelsInput(e.target.value)}
          placeholder="* or claude-sonnet,gpt-4o"
        />
      </div>
      <div className="grid grid-cols-3 gap-3">
        <div className="space-y-1.5">
          <Label htmlFor="c-rpm">RPM *</Label>
          <Input
            id="c-rpm"
            type="number"
            min={1}
            value={form.rpm}
            onChange={field('rpm')}
          />
        </div>
        <div className="space-y-1.5">
          <Label htmlFor="c-burst">Burst *</Label>
          <Input
            id="c-burst"
            type="number"
            min={1}
            value={form.burst}
            onChange={field('burst')}
          />
        </div>
        <div className="space-y-1.5">
          <Label htmlFor="c-ttl">TTL (sec) *</Label>
          <Input
            id="c-ttl"
            type="number"
            min={1}
            value={form.token_ttl_seconds}
            onChange={field('token_ttl_seconds')}
          />
        </div>
      </div>
      {error && <p className="text-sm text-red-600">{error}</p>}
      <div className="flex justify-end gap-2 pt-2">
        <Button
          type="submit"
          disabled={mut.isPending}
        >
          {mut.isPending ? 'Creating…' : 'Create client'}
        </Button>
      </div>
    </form>
  )
}

function ApiKeyModal({
  apiKey,
  onClose,
}: {
  apiKey: string
  onClose: () => void
}) {
  const [copied, setCopied] = useState(false)

  function copy() {
    navigator.clipboard.writeText(apiKey).then(() => {
      setCopied(true)
      setTimeout(() => setCopied(false), 2000)
    })
  }

  return (
    <Dialog open title="Client created — save your API key" onClose={onClose}>
      <div className="space-y-4">
        <div className="rounded-md bg-yellow-50 border border-yellow-200 px-4 py-3 text-sm text-yellow-800">
          This key is shown <strong>once</strong>. Copy it now — you won't be
          able to retrieve it later.
        </div>
        <div className="flex items-center gap-2">
          <code className="flex-1 rounded bg-gray-100 px-3 py-2 text-sm font-mono break-all">
            {apiKey}
          </code>
          <Button variant="secondary" size="sm" onClick={copy}>
            {copied ? 'Copied!' : 'Copy'}
          </Button>
        </div>
        <div className="flex justify-end">
          <Button onClick={onClose}>Done</Button>
        </div>
      </div>
    </Dialog>
  )
}

export default function Clients() {
  const qc = useQueryClient()
  const [showCreate, setShowCreate] = useState(false)
  const [newApiKey, setNewApiKey] = useState<string | null>(null)
  const [revokeId, setRevokeId] = useState<string | null>(null)

  const { data, isLoading, isError } = useQuery<Client[]>({
    queryKey: ['clients'],
    queryFn: () => api.clients.list(1000),
  })

  const revokeMut = useMutation({
    mutationFn: (id: string) => api.clients.revoke(id),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: ['clients'] })
      setRevokeId(null)
    },
  })

  function handleCreated(res: CreateClientResponse) {
    setShowCreate(false)
    setNewApiKey(res.api_key)
    qc.invalidateQueries({ queryKey: ['clients'] })
  }

  const revokeClient = data?.find((c) => c.id === revokeId)

  return (
    <div className="space-y-6">
      <div className="flex items-center justify-between">
        <div>
          <h1 className="text-2xl font-bold text-gray-900">Clients</h1>
          <p className="text-sm text-gray-500 mt-1">
            API clients that can exchange keys for JWTs
          </p>
        </div>
        <Button onClick={() => setShowCreate(true)}>
          <Plus className="h-4 w-4" />
          New client
        </Button>
      </div>

      <Card>
        {isLoading && (
          <div className="px-6 py-8 text-center text-sm text-gray-500">
            Loading…
          </div>
        )}
        {isError && (
          <div className="px-6 py-8 text-center text-sm text-red-500">
            Failed to load clients.
          </div>
        )}
        {data && (
          <div className="overflow-x-auto">
            <table className="w-full text-sm">
              <thead className="bg-gray-50 text-xs text-gray-500 uppercase tracking-wide">
                <tr>
                  <th className="px-6 py-3 text-left">Name</th>
                  <th className="px-6 py-3 text-left">Models</th>
                  <th className="px-6 py-3 text-left">RPM / Burst</th>
                  <th className="px-6 py-3 text-left">TTL</th>
                  <th className="px-6 py-3 text-left">Created</th>
                  <th className="px-6 py-3 text-left">Status</th>
                  <th className="px-6 py-3 text-right">Actions</th>
                </tr>
              </thead>
              <tbody className="divide-y divide-gray-100">
                {data.length === 0 && (
                  <tr>
                    <td
                      colSpan={7}
                      className="px-6 py-8 text-center text-gray-500"
                    >
                      No clients yet. Create one to get started.
                    </td>
                  </tr>
                )}
                {data.map((c) => (
                  <tr key={c.id} className="hover:bg-gray-50">
                    <td className="px-6 py-3">
                      <Link
                        to={`/clients/${c.id}`}
                        className="font-medium text-indigo-600 hover:text-indigo-800"
                      >
                        {c.name}
                      </Link>
                      {c.description && (
                        <p className="text-xs text-gray-400 mt-0.5">
                          {c.description}
                        </p>
                      )}
                    </td>
                    <td className="px-6 py-3 text-gray-600">
                      {c.allowed_models.join(', ')}
                    </td>
                    <td className="px-6 py-3 text-gray-600">
                      {c.rpm} / {c.burst}
                    </td>
                    <td className="px-6 py-3 text-gray-600">
                      {c.token_ttl_seconds}s
                    </td>
                    <td className="px-6 py-3 text-gray-600">
                      {fmtDate(c.created_at)}
                    </td>
                    <td className="px-6 py-3">
                      <Badge variant={c.active ? 'green' : 'red'}>
                        {c.active ? 'active' : 'revoked'}
                      </Badge>
                    </td>
                    <td className="px-6 py-3 text-right">
                      {c.active && (
                        <Button
                          variant="ghost"
                          size="sm"
                          onClick={() => setRevokeId(c.id)}
                          className="text-red-600 hover:bg-red-50"
                        >
                          <Trash2 className="h-3.5 w-3.5" />
                          Revoke
                        </Button>
                      )}
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
      </Card>

      {/* Create dialog */}
      <Dialog
        open={showCreate}
        onClose={() => setShowCreate(false)}
        title="Create client"
      >
        <CreateClientForm onCreated={handleCreated} />
      </Dialog>

      {/* API key reveal */}
      {newApiKey && (
        <ApiKeyModal apiKey={newApiKey} onClose={() => setNewApiKey(null)} />
      )}

      {/* Revoke confirmation */}
      <Dialog
        open={Boolean(revokeId)}
        onClose={() => setRevokeId(null)}
        title="Revoke client"
      >
        <div className="space-y-4">
          <p className="text-sm text-gray-600">
            Are you sure you want to revoke{' '}
            <strong>{revokeClient?.name}</strong>? All future token exchanges
            will be rejected. This cannot be undone.
          </p>
          {revokeMut.isError && (
            <p className="text-sm text-red-600">
              {(revokeMut.error as Error).message}
            </p>
          )}
          <div className="flex justify-end gap-2">
            <Button
              variant="secondary"
              onClick={() => setRevokeId(null)}
            >
              Cancel
            </Button>
            <Button
              variant="danger"
              disabled={revokeMut.isPending}
              onClick={() => revokeId && revokeMut.mutate(revokeId)}
            >
              {revokeMut.isPending ? 'Revoking…' : 'Revoke'}
            </Button>
          </div>
        </div>
      </Dialog>
    </div>
  )
}
