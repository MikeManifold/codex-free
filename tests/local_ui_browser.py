"""Browser tests against the actual replacement widget, not the user's ChatGPT tab."""
import base64
import io
import json
import os
import shutil
import tempfile
from pathlib import Path
from PIL import Image
from playwright.sync_api import sync_playwright, expect

ROOT = Path(__file__).resolve().parents[1]
HTML = (ROOT / 'src/local_ui.html').read_text()
OUTPUT = Path(tempfile.mkdtemp(prefix='render-ui-tests-'))
KEY = 'io.github.hypnguyen1209/local-ui'
passed=[]

def envelope(payload): return {'_meta': {KEY: payload}, 'content':[{'type':'text','text':'Prepared'}], 'isError':False}

def image_data(fmt):
    out=io.BytesIO(); Image.new('RGB',(2,2),(0,100,200)).save(out,format=fmt)
    return base64.b64encode(out.getvalue()).decode()

with sync_playwright() as p:
    browser=p.chromium.launch(executable_path=os.environ.get('CHROMIUM_EXECUTABLE') or shutil.which('chromium'),headless=True,args=['--no-sandbox'])
    def setup(ack=True):
        page=browser.new_page(viewport={'width':1000,'height':800})
        page.clock.install()
        page.set_content('<html><body><iframe id="widget" style="width:900px;height:700px"></iframe></body></html>')
        page.evaluate('''([source,ack]) => {
          const widget=document.getElementById('widget');
          window.addEventListener('message',event=>{
            if (event.source===widget.contentWindow && event.data?.method==='ui/initialize' && ack) {
              widget.contentWindow.postMessage({jsonrpc:'2.0',id:event.data.id,result:{hostContext:{theme:'light'}}},'*');
            }
          });
          window.emit = value => widget.contentWindow.postMessage({jsonrpc:'2.0',method:'ui/notifications/tool-result',params:value},'*');
          widget.srcdoc=source;
        }''',[HTML,ack])
        frame=page.frame_locator('#widget')
        expect(frame.locator('#title')).to_have_text('Preparing interactive view…')
        return page,frame
    def run(name,fn):
        page,frame=setup()
        try:
            fn(page,frame); passed.append(name); print('PASS',name)
        finally: page.close()
    def emit(page,payload): page.evaluate('window.emit',envelope(payload))

    def html_test(page,frame):
        emit(page,{'version':1,'kind':'html','html':'<!doctype html><h1 id="original">R14-compatible HTML path</h1>','height':350})
        expect(frame.frame_locator('#frame').locator('#original')).to_have_text('R14-compatible HTML path')
        expect(frame.locator('#notice')).to_be_hidden()
        expect(frame.locator('#frame')).to_have_css('height','350px')
    run('legacy HTML payload renders in unchanged sandbox path',html_test)

    for fmt,mime in [('PNG','image/png'),('JPEG','image/jpeg'),('GIF','image/gif'),('WEBP','image/webp'),('BMP','image/bmp')]:
        def check(page,frame,fmt=fmt,mime=mime):
            emit(page,{'kind':'image','mimeType':mime,'data':image_data(fmt)})
            expect(frame.locator('#preview img')).to_be_visible()
            expect(frame.locator('#notice')).to_be_hidden()
            assert frame.locator('#preview img').evaluate('(img)=>img.naturalWidth')==2
            expect(frame.locator('#frame')).to_be_hidden()
        run(f'{fmt} decoded as an image without nested content frame',check)

    def svg_test(page,frame):
        svg='<svg xmlns="http://www.w3.org/2000/svg" width="20" height="20"><script>window.svgRan=true</script><rect width="20" height="20" fill="blue"/></svg>'
        emit(page,{'kind':'image','mimeType':'image/svg+xml','data':base64.b64encode(svg.encode()).decode()})
        expect(frame.locator('#notice')).to_be_hidden()
        assert frame.locator('#preview img').evaluate('(img)=>img.naturalWidth')==20
        assert frame.locator('body').evaluate('()=>window.svgRan') is None
        assert frame.locator('#preview svg').count()==0
    run('SVG uses image context rather than executable inline SVG',svg_test)

    def text_test(page,frame):
        text='<img src=x onerror="window.executed=true">\n# Markdown source 中文'
        emit(page,{'kind':'text','text':text})
        expect(frame.locator('pre')).to_have_text(text)
        assert frame.locator('#preview img').count()==0
        assert frame.locator('body').evaluate('()=>window.executed') is None
    run('text/Markdown source escapes markup instead of executing it',text_test)

    def json_test(page,frame):
        source='{"id":9007199254740993,"html":"<script>evil()</script>"}'
        emit(page,{'kind':'json','text':source})
        expect(frame.locator('pre')).to_have_text(source)
    run('JSON preserves integer precision and source text',json_test)

    def csv_test(page,frame):
        emit(page,{'kind':'csv','text':'\ufeffname,value,empty\r\n"a,b","line 1\nline ""2""",\r\n'})
        expect(frame.locator('tbody tr')).to_have_count(2)
        assert frame.locator('tbody tr').nth(1).locator('td').all_text_contents()==['a,b','line 1\nline "2"','']
    run('CSV handles BOM CRLF quotes multiline and trailing empty cell',csv_test)

    def tsv_test(page,frame):
        emit(page,{'kind':'tsv','text':'name\tvalue\n中文\t"a\tb"'})
        expect(frame.locator('tbody tr')).to_have_count(2)
        assert frame.locator('tbody tr').nth(1).locator('td').all_text_contents()==['中文','a\tb']
    run('TSV preserves embedded quoted tabs and Unicode',tsv_test)

    def xss_table(page,frame):
        emit(page,{'kind':'csv','text':'<script>window.evil=true</script>,=1+2'})
        expect(frame.locator('td').first).to_have_text('<script>window.evil=true</script>')
        assert frame.locator('#preview script').count()==0
        assert frame.locator('td').nth(1).inner_text()=='=1+2'
    run('table cells are inert text; formulas and markup never execute',xss_table)

    for name,payload,expected in [
        ('malformed CSV ends waiting',{'kind':'csv','text':'"unclosed'},'Unclosed'),
        ('invalid JSON ends waiting',{'kind':'json','text':'bad'},'JSON'),
        ('unsupported image type ends waiting',{'kind':'image','mimeType':'text/html','data':'YQ=='},'MIME'),
        ('invalid base64 ends waiting',{'kind':'image','mimeType':'image/png','data':'!@#$'},'base64'),
        ('table row limit is explicit',{'kind':'csv','text':'x\n'*2001},'2000'),
        ('unknown preview kind is explicit',{'kind':'pdf','text':'test'},'supported'),
    ]:
        def invalid(page,frame,payload=payload,expected=expected):
            emit(page,payload)
            expect(frame.locator('#title')).to_have_text('Preview unavailable')
            expect(frame.locator('#notice')).to_contain_text(expected)
            expect(frame.locator('#frame')).to_be_hidden()
            assert frame.locator('#shell').evaluate('el=>el.getBoundingClientRect().height')<180
        run(name,invalid)

    def ordinary(page,frame):
        page.evaluate('window.emit',{'content':[{'type':'image','mimeType':'image/png','data':image_data('PNG')}], 'isError':False})
        expect(frame.locator('#title')).to_have_text('No preview data')
        expect(frame.locator('#frame')).to_be_hidden()
    run('legacy accidental screenshot-only widget finishes empty',ordinary)

    def fail(page,frame):
        page.evaluate('window.emit',{'result':{'isError':True,'content':[{'type':'text','text':'failed'}]}})
        expect(frame.locator('#title')).to_have_text('Preview failed')
        page.clock.fast_forward(31000)
        expect(frame.locator('#title')).to_have_text('Preview failed')
    run('canonical error is terminal and not replaced by timeout',fail)

    def nested(page,frame):
        page.evaluate('window.emit',{'result':{'call_tool_result':envelope({'kind':'text','text':'canonical metadata'})}})
        expect(frame.locator('pre')).to_have_text('canonical metadata')
    run('canonical wrapped result metadata is recognized',nested)

    def late(page,frame):
        page.clock.fast_forward(31000)
        expect(frame.locator('#title')).to_have_text('No preview received')
        emit(page,{'kind':'text','text':'late result'})
        expect(frame.locator('pre')).to_have_text('late result')
        expect(frame.locator('#notice')).to_be_hidden()
    run('late valid result recovers after delivery timeout',late)

    def empty_late(page,frame):
        page.evaluate('window.emit',{'content':[{'type':'text','text':'ordinary'}]})
        expect(frame.locator('#title')).to_have_text('No preview data')
        emit(page,{'kind':'text','text':'later valid result'})
        expect(frame.locator('pre')).to_have_text('later valid result')
    run('later metadata can recover after an empty result',empty_late)

    def invalid_image(page,frame):
        emit(page,{'kind':'image','mimeType':'image/png','data':'dGVzdA=='})
        expect(frame.locator('#title')).to_have_text('Preview unavailable')
        expect(frame.locator('#notice')).to_contain_text('decoded')
    run('image decode failure shows explicit error',invalid_image)

    def height(page,frame):
        emit(page,{'kind':'text','text':'large height','height':100000})
        expect(frame.locator('#preview')).to_have_css('max-height','1000px')
    run('viewport height clamps at 1000 pixels',height)

    page,frame=setup(ack=False)
    page.clock.fast_forward(11000)
    expect(frame.locator('#title')).to_have_text('Host connection unavailable')
    emit(page,{'kind':'text','text':'fallback result'})
    expect(frame.locator('pre')).to_have_text('fallback result')
    passed.append('initialization timeout remains recoverable'); print('PASS',passed[-1]); page.close()

    # Keep a screenshot only of synthetic sample data, not user project data.
    page,frame=setup()
    emit(page,{'kind':'csv','title':'render_ui · CSV preview','sourcePath':'sample.csv','text':'name,format,status\nR14,HTML,preserved\nScreenshot,PNG,separate tool output\nData,CSV,preview only on request'})
    expect(frame.locator('table')).to_be_visible()
    page.screenshot(path=str(OUTPUT/'browser-preview.png'))
    page.close(); browser.close()
(OUTPUT/'browser_results.json').write_text(json.dumps({'passed':len(passed),'tests':passed,'host':'isolated Chromium with simulated MCP Apps parent; not ChatGPT','scope':'browser tests only; does not replace cargo test or ChatGPT end-to-end checks'},ensure_ascii=False,indent=2))
print(f'PASSED {len(passed)} browser tests')

print(f'Test artifacts: {OUTPUT}')
