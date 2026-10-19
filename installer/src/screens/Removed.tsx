import { motion } from 'framer-motion'
import { HwWaveMark, Bars } from '../components/HwWaveMark'

interface Props {
  deletedLocalCopies: boolean
  onClose: () => void
}

/** Uninstall 3: done, and where to get it back. */
export function Removed({ deletedLocalCopies, onClose }: Props) {
  return (
    <motion.div initial={{ opacity: 0, scale: 0.97 }} animate={{ opacity: 1, scale: 1 }} exit={{ opacity: 0 }} transition={{ duration: 0.4, ease: [0.16, 1, 0.3, 1] }} className="screen">
      <div className="ib center">
        <HwWaveMark />
        <div className="cap ok">Removed</div>
        <h1 className="t big">Hardwave Workspace is gone.</h1>
        <p className="sub">
          Get it back any time at hardwavestudios.com.
          <br />
          {deletedLocalCopies
            ? 'Your files are still online. Sign in and they come back to this PC.'
            : 'Your files are still online, and your Workspace folder stays on this PC.'}
        </p>
        <div className="ft center" style={{ marginTop: 18 }}>
          <button type="button" className="ibtn" onClick={onClose}>Close</button>
        </div>
      </div>
      <Bars faint />
    </motion.div>
  )
}
