import { useState } from 'react'
import { useQuery, useMutation, useQueryClient } from '@tanstack/react-query'
import { RotateCcw } from 'lucide-react'
import { api, type SigningKey } from '../api'
import { Card } from '../components/ui/Card'
import Button from '../components/ui/Button'
import Badge from '../components/ui/Badge'
import Dialog from '../components/ui/Dialog'

function keyStatus(k: SigningKey): { label: string; variant: 'green' | 'yellow' | 'gray' } {
  if (!k.active) return { label: 'retired', variant: 'gray' }
  if (k.retired_at) return { label: 'retiring', variant: 'yellow' }
  return { label: 'active', variant: 'green' }
}

function fmtDate(iso: string) {
  return new Date(iso).toLocaleString()
}

export default function SigningKeys() {
  const qc = useQueryClient()
  const [confirm, setConfirm] = useState(false)
  const [rotateError, setRotateError] = useState('')

  const { data, isLoading, isError } = useQuery<SigningKey[]>({
    queryKey: ['signing-keys'],
    queryFn: () => api.signingKeys.list(),
  })

  const rotateMut = useMutation({
    mutationFn: api.signingKeys.rotate,
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: ['signing-keys'] })
      setConfirm(false)
    },
    onError: (e: Error) => setRotateError(e.message),
  })

  return (
    <div className="space-y-6">
      <div className="flex items-center justify-between">
        <div>
          <h1 className="text-2xl font-bold text-gray-900">Signing Keys</h1>
          <p className="text-sm text-gray-500 mt-1">
            RSA-2048 keypairs used to sign JWTs
          </p>
        </div>
        <Button onClick={() => { setRotateError(''); setConfirm(true) }}>
          <RotateCcw className="h-4 w-4" />
          Rotate keys
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
            Failed to load signing keys.
          </div>
        )}
        {data && (
          <div className="overflow-x-auto">
            <table className="w-full text-sm">
              <thead className="bg-gray-50 text-xs text-gray-500 uppercase tracking-wide">
                <tr>
                  <th className="px-6 py-3 text-left">Key ID (kid)</th>
                  <th className="px-6 py-3 text-left">Algorithm</th>
                  <th className="px-6 py-3 text-left">Created</th>
                  <th className="px-6 py-3 text-left">Retires at</th>
                  <th className="px-6 py-3 text-left">Status</th>
                </tr>
              </thead>
              <tbody className="divide-y divide-gray-100">
                {data.length === 0 && (
                  <tr>
                    <td
                      colSpan={5}
                      className="px-6 py-8 text-center text-gray-500"
                    >
                      No signing keys.
                    </td>
                  </tr>
                )}
                {data.map((k) => {
                  const { label, variant } = keyStatus(k)
                  return (
                    <tr key={k.kid} className="hover:bg-gray-50">
                      <td className="px-6 py-3 font-mono text-xs text-gray-700">
                        {k.kid.slice(0, 8)}…
                      </td>
                      <td className="px-6 py-3 text-gray-600">{k.algorithm}</td>
                      <td className="px-6 py-3 text-gray-600 text-xs whitespace-nowrap">
                        {fmtDate(k.created_at)}
                      </td>
                      <td className="px-6 py-3 text-gray-600 text-xs whitespace-nowrap">
                        {k.retired_at ? fmtDate(k.retired_at) : '—'}
                      </td>
                      <td className="px-6 py-3">
                        <Badge variant={variant}>{label}</Badge>
                      </td>
                    </tr>
                  )
                })}
              </tbody>
            </table>
          </div>
        )}
      </Card>

      <Dialog
        open={confirm}
        onClose={() => setConfirm(false)}
        title="Rotate signing keys"
      >
        <div className="space-y-4">
          <p className="text-sm text-gray-600">
            A new RSA-2048 keypair will be generated. The current active key(s)
            will be scheduled for retirement after the longest active client
            token TTL — both keys remain in the JWKS during this overlap window
            so in-flight tokens stay valid.
          </p>
          {rotateError && (
            <p className="text-sm text-red-600">{rotateError}</p>
          )}
          <div className="flex justify-end gap-2">
            <Button variant="secondary" onClick={() => setConfirm(false)}>
              Cancel
            </Button>
            <Button
              disabled={rotateMut.isPending}
              onClick={() => rotateMut.mutate()}
            >
              {rotateMut.isPending ? 'Rotating…' : 'Rotate'}
            </Button>
          </div>
        </div>
      </Dialog>
    </div>
  )
}
