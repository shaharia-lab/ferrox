import { Routes, Route, Navigate } from 'react-router-dom'
import { useState } from 'react'
import Layout from './components/Layout'
import Login from './pages/Login'
import Dashboard from './pages/Dashboard'
import Clients from './pages/Clients'
import ClientDetail from './pages/ClientDetail'
import SigningKeys from './pages/SigningKeys'
import AuditLog from './pages/AuditLog'
import { getAdminKey } from './api'

export default function App() {
  const [authed, setAuthed] = useState(() => Boolean(getAdminKey()))

  if (!authed) {
    return <Login onLogin={() => setAuthed(true)} />
  }

  return (
    <Layout onLogout={() => setAuthed(false)}>
      <Routes>
        <Route path="/" element={<Dashboard />} />
        <Route path="/clients" element={<Clients />} />
        <Route path="/clients/:id" element={<ClientDetail />} />
        <Route path="/signing-keys" element={<SigningKeys />} />
        <Route path="/audit" element={<AuditLog />} />
        <Route path="*" element={<Navigate to="/" replace />} />
      </Routes>
    </Layout>
  )
}
