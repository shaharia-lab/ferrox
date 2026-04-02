import { NavLink } from 'react-router-dom'
import { ReactNode } from 'react'
import {
  LayoutDashboard,
  Users,
  Key,
  ClipboardList,
  LogOut,
  Shield,
} from 'lucide-react'
import { clearAdminKey } from '../api'

const navItems = [
  { to: '/', label: 'Dashboard', icon: LayoutDashboard, end: true },
  { to: '/clients', label: 'Clients', icon: Users },
  { to: '/signing-keys', label: 'Signing Keys', icon: Key },
  { to: '/audit', label: 'Audit Log', icon: ClipboardList },
]

export default function Layout({
  children,
  onLogout,
}: {
  children: ReactNode
  onLogout: () => void
}) {
  function handleLogout() {
    clearAdminKey()
    onLogout()
  }

  return (
    <div className="min-h-screen flex">
      {/* Sidebar */}
      <aside className="w-56 bg-gray-900 flex flex-col shrink-0">
        <div className="flex items-center gap-2 px-5 py-5 border-b border-gray-700">
          <Shield className="h-6 w-6 text-indigo-400" />
          <span className="text-white font-semibold text-sm">Ferrox CP</span>
        </div>
        <nav className="flex-1 px-3 py-4 space-y-1">
          {navItems.map(({ to, label, icon: Icon, end }) => (
            <NavLink
              key={to}
              to={to}
              end={end}
              className={({ isActive }) =>
                `flex items-center gap-2.5 px-3 py-2 rounded-md text-sm transition-colors ${
                  isActive
                    ? 'bg-indigo-700 text-white'
                    : 'text-gray-300 hover:bg-gray-700 hover:text-white'
                }`
              }
            >
              <Icon className="h-4 w-4" />
              {label}
            </NavLink>
          ))}
        </nav>
        <div className="px-3 py-4 border-t border-gray-700">
          <button
            onClick={handleLogout}
            className="flex items-center gap-2.5 px-3 py-2 rounded-md text-sm text-gray-300 hover:bg-gray-700 hover:text-white w-full transition-colors cursor-pointer"
          >
            <LogOut className="h-4 w-4" />
            Sign out
          </button>
        </div>
      </aside>

      {/* Main content */}
      <main className="flex-1 overflow-auto">
        <div className="max-w-6xl mx-auto px-6 py-8">{children}</div>
      </main>
    </div>
  )
}
