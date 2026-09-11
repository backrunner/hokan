import type { OgTemplate, OgTemplateNode } from 'svedocs/og';

const text = (children: string, style: Record<string, string | number>): OgTemplateNode => ({
  type: 'div', props: { children, style }
});

export const hokanOgTemplate: OgTemplate = ({ title, description }) => {
  const isHome = title === 'Hokan — shell-aware terminal completion';
  const heading = isHome ? 'Completion for\nzsh, bash & fish.' : title;
  return {
    type: 'div',
    props: {
      style: { display: 'flex', flexDirection: 'column', width: 1200, height: 630, padding: '48px 64px', background: '#faf9f6', color: '#242824', fontFamily: 'Inter' },
      children: [
        {
          type: 'div', props: {
            style: { display: 'flex', alignItems: 'center', gap: 14, paddingBottom: 28, borderBottom: '1px solid #dedfd7' },
            children: [
              {
                type: 'svg', props: {
                  width: 38, height: 38, viewBox: '0 0 64 64',
                  children: [
                    { type: 'rect', props: { width: 64, height: 64, rx: 8, fill: '#243a2d' } },
                    { type: 'path', props: { d: 'M16 16h8v12h16V16h8v32h-8V36H24v12h-8z', fill: '#d8e5d6' } },
                    { type: 'path', props: { d: 'M35 51h14v4H35z', fill: '#d3b585' } }
                  ]
                }
              },
              text('hokan', { fontSize: 28, letterSpacing: '-1px' }),
              text('Documentation', { marginLeft: 'auto', color: '#666b64', fontSize: 20 })
            ]
          }
        },
        text(heading, { fontSize: heading.length > 50 ? 57 : 72, letterSpacing: '-3px', lineHeight: 1.1, marginTop: 45, whiteSpace: 'pre-wrap', maxWidth: 1040 }),
        text(isHome ? 'Inline suggestions from your history, files, and project scripts.' : (description ?? 'Installation, guides, and reference for Hokan.'), { color: '#666b64', fontSize: 25, lineHeight: 1.45, marginTop: 22, maxWidth: 950 }),
        {
          type: 'div', props: {
            style: { display: 'flex', alignItems: 'center', marginTop: 'auto', borderTop: '1px solid #dedfd7', paddingTop: 24 },
            children: [
              text('Shell-aware terminal completion', { fontSize: 18, color: '#666b64' }),
              text('hokan.pwp.sh', { marginLeft: 'auto', fontSize: 18, color: '#38644b' })
            ]
          }
        }
      ]
    }
  };
};
