import { motion } from 'framer-motion'

interface Props {
  /** The Workspace folder on this PC; empty when unknown, null while loading. */
  folder: string | null
  deleteLocalCopies: boolean
  setDeleteLocalCopies: (v: boolean) => void
  onCancel: () => void
  onRemove: () => void
}

/** Uninstall 1: what stays and what goes. Nothing is removed before Remove. */
export function Remove({ folder, deleteLocalCopies, setDeleteLocalCopies, onCancel, onRemove }: Props) {
  const stays = [
    'Your files online in Workspace',
    'Your Hardwave account',
    ...(deleteLocalCopies ? [] : ['Your Workspace folder and the files in it']),
    'Your Workspace settings, for a later install',
  ]
  const goes = [
    'The Hardwave Workspace app',
    'Its desktop and Start menu shortcuts',
    'Its entry in the Windows apps list',
    ...(deleteLocalCopies ? ['The downloaded copies in your Workspace folder'] : []),
  ]

  return (
    <motion.div initial={{ opacity: 0, x: 16 }} animate={{ opacity: 1, x: 0 }} exit={{ opacity: 0, x: -16 }} transition={{ duration: 0.3 }} className="screen">
      <div className="ib">
        <div className="cap">Uninstall</div>
        <h1 className="t">Remove Hardwave Workspace?</h1>
        <p className="sub">Nothing is removed until you press Remove.</p>
        <div className="sg">
          <div className="stays">
            <h2>Stays</h2>
            <ul>{stays.map((s) => <li key={s}>{s}</li>)}</ul>
          </div>
          <div className="goes">
            <h2>Goes</h2>
            <ul>{goes.map((g) => <li key={g}>{g}</li>)}</ul>
          </div>
        </div>
        <button type="button" className="opt two" onClick={() => setDeleteLocalCopies(!deleteLocalCopies)} aria-pressed={deleteLocalCopies}>
          <i className={deleteLocalCopies ? 'on' : ''} />
          <span>
            Also delete the downloaded copies in your Workspace folder
            <small>{folder ? `${folder} · ` : ''}Your files stay online. Uploads finish first.</small>
          </span>
        </button>
        <div className="ft">
          <button type="button" className="ibtn ghost" onClick={onCancel}>Cancel</button>
          <button type="button" className="ibtn" onClick={onRemove}>Remove</button>
        </div>
      </div>
    </motion.div>
  )
}
