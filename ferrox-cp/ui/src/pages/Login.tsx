import { FormEvent, useState } from 'react'
import { Shield } from 'lucide-react'
import { setAdminKey } from '../api'
import Button from '../components/ui/Button'
import Input from '../components/ui/Input'
import Label from '../components/ui/Label'

export default function Login({ onLogin }: { onLogin: () => void }) {
  const [key, setKey] = useState('')
  const [error, setError] = useState('')

  async function handleSubmit(e: FormEvent) {
    e.preventDefault()
    if (!key.trim()) {
      setError('Admin key is required')
      return
    }
    setAdminKey(key.trim())
    // Verify by hitting the API
    try {
      const res = await fetch('/api/clients?limit=1', {
        headers: { Authorization: `Bearer ${key.trim()}` },
      })
      if (res.status === 401 || res.status === 403) {
        setError('Invalid admin key')
        return
      }
      onLogin()
    } catch {
      setError('Could not reach the control plane API')
    }
  }

  return (
    <div className="min-h-screen flex items-center justify-center bg-gray-50">
      <div className="w-full max-w-sm">
        <div className="flex flex-col items-center mb-8">
          <Shield className="h-12 w-12 text-indigo-600 mb-3" />
          <h1 className="text-2xl font-bold text-gray-900">Ferrox Control Plane</h1>
          <p className="text-sm text-gray-500 mt-1">Sign in with your admin key</p>
        </div>
        <form onSubmit={handleSubmit} className="bg-white shadow rounded-lg p-6 space-y-4">
          <div className="space-y-1.5">
            <Label htmlFor="key">Admin Key</Label>
            <Input
              id="key"
              type="password"
              placeholder="CP_ADMIN_KEY value"
              value={key}
              onChange={(e) => {
                setKey(e.target.value)
                setError('')
              }}
              autoFocus
            />
          </div>
          {error && <p className="text-sm text-red-600">{error}</p>}
          <Button type="submit" className="w-full justify-center">
            Sign in
          </Button>
        </form>
      </div>
    </div>
  )
}
