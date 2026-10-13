import { motion } from 'framer-motion'
import { HwWaveMark, Bars, Wordmark } from '../components/HwWaveMark'

interface Props {
  onLaunch: () => void
  onClose: () => void
}

export function Done({ onLaunch, onClose }: Props) {
  return (
    <motion.div initial={{ opacity: 0, scale: 0.97 }} animate={{ opacity: 1, scale: 1 }} exit={{ opacity: 0 }} transition={{ duration: 0.4, ease: [0.16, 1, 0.3, 1] }} className="screen">
      <div className="ib center">
        <HwWaveMark />
        <Wordmark />
        <div className="cap ok" style={{ marginTop: 10 }}>Installed</div>
        <h1 className="t big">Sign in next.</h1>
        <p className="sub">Pick the folder you want backed up; the rest happens on its own.</p>
        <div className="ft center" style={{ marginTop: 18 }}>
          <button type="button" className="ibtn ghost" onClick={onClose}>Close</button>
          <button type="button" className="ibtn" onClick={onLaunch}>Open Workspace</button>
        </div>
      </div>
      <Bars faint />
    </motion.div>
  )
}
