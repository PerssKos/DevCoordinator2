'use strict';

window.DevCoordinatorTickets = (() => {
  const esc = value => String(value ?? '').replace(/[&<>"']/g, c => ({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[c]));
  const icon = name => `<span class="ti ti-${name}" aria-hidden="true"></span>`;
  const size = n => n < 1024 ? `${n} B` : n < 1048576 ? `${(n/1024).toFixed(1)} KiB` : `${(n/1048576).toFixed(1)} MiB`;
  const drafts = new Map();
  const uuid = () => crypto.randomUUID();
  const digest = async bytes => Array.from(new Uint8Array(await crypto.subtle.digest('SHA-256',bytes)), b=>b.toString(16).padStart(2,'0')).join('');
  const readFile = file => file.arrayBuffer();
  const bytes64 = data => { let value=''; for(const b of data)value+=String.fromCharCode(b); return btoa(value); };
  const from64 = data => Uint8Array.from(atob(data),c=>c.charCodeAt(0));
  const t = (key, fallback) => window.DevCoordinatorI18n?.t(`tickets.${key}`) || fallback;
  const text = (key,fallback) => window.DevCoordinatorI18n ? `<span data-i18n="tickets.${key}">${esc(t(key,fallback))}</span>` : esc(fallback);

  async function mount(root, {api, publicOnly=false, administrator=false, signal}={}) {
    await window.DevCoordinatorI18n?.ensure('tickets');
    let active=true, sequence=0, target='local', view=publicOnly?'received':'submitted', selected=null, settings=null;
    let tickets=[], nextList=null, filter='', status='', detail=null, comments=[], nextComments=null, firstCommentOffset=0;
    const urls=new Set();
    const dialogs=new Set();
    const abort=()=>{active=false;for(const u of urls)URL.revokeObjectURL(u);for(const d of dialogs)d.remove();};
    signal?.addEventListener('abort',abort,{once:true});
    const call = action => api('ticket.request',{target,action});
    const draftKey=()=>`${target}:${selected || 'new'}`;
    const draft = () => { if(!drafts.has(draftKey()))drafts.set(draftKey(),{body:'',files:[],key:uuid()});return drafts.get(draftKey()); };
    const error = (message,node=root.querySelector('.ticket-error')) => {if(node){node.textContent=message;node.hidden=!message;}};
    const run = async (button,work) => {if(button)button.disabled=true;error('');try{await work();if(active)error('');}catch(e){if(active)error(e.message);}finally{if(button?.isConnected)button.disabled=false;}};
    const label = value => { try{return new URL(value).host;}catch{return value;} };
    const stamp = value => new Date(value).toLocaleString(window.DevCoordinatorI18n?.locale || undefined,{dateStyle:'medium',timeStyle:'short'});
    const marked = value => `<span class="ticket-state ${value?'closed':'open'}">${value?text('closed','Closed'):text('open','Open')}</span>`;
    const fileMarkup = (files,commentId=null) => files.length ? `<div class="ticket-files">${files.map(file=>`<button type="button" class="ticket-file" data-file="${esc(file.id)}" data-comment="${esc(commentId||'')}" aria-label="${esc(t('view','View')+' '+file.name)}">${icon(file.content_type.startsWith('image/')?'photo':'file-text')}<span><strong>${esc(file.name)}</strong><small>${size(file.byte_size)} · ${text('view','View')}</small></span></button>`).join('')}</div>` : '';
    const pendingMarkup = d => d.files.map((item,i)=>`<div class="ticket-pending"><span>${esc(item.file.name)} <small>${item.uploaded ? text('attached','Attached') : item.error ? esc(item.error) : text('uploading','Uploading…')}</small></span>${item.error?`<button type="button" class="btn btn-small" data-upload-retry="${i}">${text('retry','Retry')}</button>`:''}<button type="button" class="btn btn-small" data-upload-remove="${i}" aria-label="${esc(t('removeFile','Remove file')+' '+item.file.name)}">${icon('x')}</button></div>`).join('');

    function shell() {
      root.classList.add('tickets-page');
      root.innerHTML=`<div class="ticket-heading"><h1>${text('title','Feature requests')}</h1><div class="ticket-heading-actions">${!publicOnly?`<button class="btn" id="ticket-settings" aria-label="${esc(t('upstreamSettings','Upstream server'))}">${icon('settings')}<span>${text('upstream','Upstream')}: ${esc(label(settings.upstream))}</span></button><button class="btn btn-primary" id="ticket-new" aria-label="${esc(t('new','New request'))}">${icon('plus')}<span>${text('new','New request')}</span></button>`:`<a class="btn" href="/">${text('console','Open Console')}</a>`}</div></div>
        <div class="ticket-toolbar">${!publicOnly?`<div class="ticket-tabs" aria-label="${esc(t('requestView','Request view'))}"><button class="btn ${view==='submitted'?'selected':''}" data-ticket-view="submitted">${text('submitted','Submitted')}</button><button class="btn ${view==='received'?'selected':''}" data-ticket-view="received">${text('received','Received')}</button></div>`:`<span>${text('public','Public tickets and attachments')}</span>`}<div class="ticket-filters"><select id="ticket-status" aria-label="${esc(t('status','Status'))}"><option value="">${t('allStatuses','All statuses')}</option><option value="open">${t('open','Open')}</option><option value="closed">${t('closed','Closed')}</option></select><button class="btn" id="ticket-refresh" aria-label="${esc(t('refresh','Refresh requests'))}">${icon('refresh')}</button></div></div>
        <p class="ticket-error" role="alert" hidden></p><div class="ticket-layout"><aside class="ticket-inbox" aria-label="${esc(t('title','Feature requests'))}"><input id="ticket-search" type="search" placeholder="${esc(t('search','Search loaded requests'))}" aria-label="${esc(t('search','Search loaded requests'))}"><div id="ticket-list"></div><button class="btn" id="ticket-more" hidden>${text('more','More requests')}</button></aside><section class="ticket-discussion" aria-label="${esc(t('discussion','Discussion'))}"><p>${text('choose','Choose a request to read its discussion.')}</p></section></div>`;
      root.querySelector('#ticket-search').value=filter;
      root.querySelector('#ticket-search').oninput=e=>{filter=e.target.value;paintList();};
      root.querySelector('#ticket-status').value=status;
      root.querySelector('#ticket-status').onchange=e=>{status=e.target.value;run(e.target,()=>loadList());};
      root.querySelector('#ticket-refresh').onclick=e=>run(e.currentTarget,async()=>{await loadList();if(selected)await open(selected);});
      root.querySelector('#ticket-more').onclick=e=>run(e.currentTarget,()=>loadList(nextList));
      root.querySelector('#ticket-new')?.addEventListener('click',()=>editor());
      root.querySelector('#ticket-settings')?.addEventListener('click',()=>configure());
      root.querySelectorAll('[data-ticket-view]').forEach(button=>button.onclick=()=>run(button,async()=>{
        if(view===button.dataset.ticketView)return;
        view=button.dataset.ticketView;target=view==='received'?'local':settings.upstream;selected=null;detail=null;comments=[];shell();await loadList();
      }));
    }

    function paintList() {
      if(!active)return;
      const filtered=tickets.filter(ticket=>ticket.title.toLowerCase().includes(filter.toLowerCase()));
      root.querySelector('#ticket-list').innerHTML=filtered.length?filtered.map(ticket=>`<button class="ticket-row ${selected===ticket.id?'selected':''}" data-ticket-id="${esc(ticket.id)}" aria-current="${selected===ticket.id?'true':'false'}"><span><strong>${esc(ticket.title)}</strong><small>${esc(stamp(ticket.updated_at))}</small></span>${marked(ticket.closed)}</button>`).join(''):`<p class="ticket-empty">${filter?text('noMatch','No matching requests on this page.'):text('empty','No requests yet.')}</p>`;
      root.querySelectorAll('[data-ticket-id]').forEach(button=>button.onclick=()=>run(button,()=>open(button.dataset.ticketId)));
      root.querySelector('#ticket-more').hidden=nextList==null;
    }
    async function loadList(offset=0) {
      const current=++sequence;
      const result=await call({kind:'list',offset:offset||0,limit:20,closed:status?status==='closed':null,mine:view==='submitted'});
      if(!active||current!==sequence)return;
      tickets=offset?[...tickets,...result.items]:result.items;nextList=result.next_offset;paintList();
    }
    async function open(id) {
      selected=id;paintList();const current=++sequence;
      const result=await call({kind:'get',ticket_id:id});
      const start=Math.max(0,result.ticket.comment_count-20);
      const thread=await commentWindow(id,start,result.ticket.comment_count);
      if(!active||current!==sequence)return;
      detail=result;comments=thread;firstCommentOffset=start;nextComments=start>0?Math.max(0,start-20):null;paintDetail();
      const query=new URLSearchParams({ticket:id,view,target});
      history.replaceState(null,'',`${publicOnly?'/requests':'#/requests'}?${query}`);
    }
    async function commentWindow(id,start,end) {
      const items=[];let offset=start;
      while(offset<end){const page=await call({kind:'comments',ticket_id:id,offset,limit:Math.min(20,end-offset)});items.push(...page.items);if(page.next_offset==null)break;if(page.next_offset<=offset)throw new Error('Discussion did not advance. Refresh to retry.');offset=page.next_offset;}
      return items;
    }
    function paintDetail() {
      const ticket=detail.ticket;
      const d=draft();
      const can=detail.can_manage&&!publicOnly;
      root.querySelector('.ticket-discussion').innerHTML=`<button class="btn ticket-back">${icon('arrow-left')}${text('back','All requests')}</button><header class="ticket-subject"><div><h2>${esc(ticket.title)}</h2><div class="ticket-byline">${text('from','From')} ${esc(ticket.author.name)} <span>·</span> ${text('owned','Owned by')} ${esc(label(ticket.upstream))}</div></div><div class="ticket-controls">${marked(ticket.closed)}${can?`<button class="btn" id="ticket-edit" aria-label="${esc(t('editRequest','Edit request'))}">${icon('pencil')}<span>${text('edit','Edit')}</span></button><details class="ticket-menu"><summary class="btn" aria-label="${esc(t('actions','Request actions'))}">${icon('chevron-down')}</summary><div><button class="btn" id="ticket-close">${ticket.closed?text('reopen','Reopen ticket'):text('close','Close ticket')}</button><button class="btn btn-danger" id="ticket-remove">${text('remove','Remove request')}</button></div></details>`:''}</div></header><div class="ticket-description">${esc(ticket.body)}</div>${fileMarkup(ticket.attachments)}<div class="ticket-comments">${comments.map(comment=>`<article class="ticket-comment"><header>${icon('user')}<strong>${esc(comment.author.name)}</strong><time datetime="${esc(comment.created_at)}">${esc(stamp(comment.created_at))}</time></header><p>${esc(comment.body)}</p>${fileMarkup(comment.attachments,comment.id)}</article>`).join('')}</div>${nextComments!=null?`<button class="btn" id="ticket-comments-more">${text('moreComments','Show earlier comments')}</button>`:''}${detail.can_comment&&!publicOnly?`<form class="ticket-composer"><label for="ticket-reply">${text('writeReply','Write a reply')}</label><textarea id="ticket-reply" maxlength="8192" rows="3">${esc(d.body)}</textarea><div class="ticket-pending-files">${pendingMarkup(d)}</div><div class="ticket-compose-actions"><button type="button" class="btn ticket-attach">${icon('plus')}${text('attach','Attach files')}</button><input type="file" multiple hidden class="ticket-file-input" aria-label="${esc(t('attach','Attach files'))}"><small>${text('publicNotice','Replies and attachments are public.')}</small><button class="btn btn-primary" type="submit">${text('send','Send reply')}</button></div></form>`:''}`;
      root.classList.add('ticket-selected');
      const earlier=root.querySelector('#ticket-comments-more');if(earlier)root.querySelector('.ticket-comments').before(earlier);
      root.querySelector('.ticket-back').onclick=()=>{root.classList.remove('ticket-selected');root.querySelector(`[data-ticket-id="${selected}"]`)?.focus();};
      root.querySelector('#ticket-edit')?.addEventListener('click',()=>editor(ticket));
      root.querySelector('#ticket-close')?.addEventListener('click',e=>run(e.currentTarget,async()=>{await call({kind:'close',ticket_id:selected,expected_revision:ticket.revision,closed:!ticket.closed});await loadList();await open(selected);}));
      root.querySelector('#ticket-remove')?.addEventListener('click',e=>run(e.currentTarget,async()=>{await call({kind:'remove',ticket_id:selected,expected_revision:ticket.revision});drafts.delete(draftKey());selected=null;detail=null;root.classList.remove('ticket-selected');shell();await loadList();}));
      root.querySelector('#ticket-comments-more')?.addEventListener('click',e=>run(e.currentTarget,async()=>{const start=nextComments;const earlier=await commentWindow(selected,start,firstCommentOffset);comments.unshift(...earlier);firstCommentOffset=start;nextComments=start>0?Math.max(0,start-20):null;paintDetail();}));
      root.querySelectorAll('[data-file]').forEach(button=>button.onclick=()=>run(button,()=>preview(button.dataset.file,button.dataset.comment||null)));
      const thumbTicket=selected;
      for(const button of [...root.querySelectorAll('[data-file]')].filter(b=>b.querySelector('.ti-photo')).slice(0,6)) {
        const commentId=button.dataset.comment||null;
        const list=commentId?comments.find(c=>c.id===commentId)?.attachments:ticket.attachments;
        const metadata=list?.find(f=>f.id===button.dataset.file);
        if(!metadata||metadata.byte_size>4*1048576)continue;
        readAttachment(thumbTicket,commentId,metadata.id).then(({blob})=>{
          if(!button.isConnected||!active)return;const url=URL.createObjectURL(blob);urls.add(url);
          const img=document.createElement('img');img.alt='';img.src=url;img.className='ticket-thumbnail';button.querySelector('.ti')?.replaceWith(img);
        }).catch(()=>{});
      }
      const form=root.querySelector('.ticket-composer');
      if(form){
        form.querySelector('textarea').oninput=e=>d.body=e.target.value;
        bindFiles(form,d);
        form.onsubmit=e=>{e.preventDefault();run(form.querySelector('[type=submit]'),async()=>{
          if(!d.body.trim()&&!d.files.length)throw new Error(t('replyRequired','Write a reply or attach a file.'));
          if(d.files.some(f=>!f.uploaded))throw new Error(t('waitFiles','Finish uploading or remove failed files before sending.'));
          await call({kind:'comment',ticket_id:selected,request_key:d.key,body:d.body,attachments:d.files.map(f=>f.id)});
          drafts.delete(draftKey());await loadList();await open(selected);root.querySelector('#ticket-reply')?.focus();
        });};
      }
    }

    function bindFiles(container,d) {
      const uploadTarget=target;
      const uploadCall=action=>api('ticket.request',{target:uploadTarget,action});
      const paint=()=>{
        if(!container.isConnected)return;
        container.querySelector('.ticket-pending-files').innerHTML=pendingMarkup(d);
        container.querySelectorAll('[data-upload-remove]').forEach(button=>button.onclick=()=>{
          const item=d.files[Number(button.dataset.uploadRemove)];item.cancelled=true;d.files=d.files.filter(f=>f!==item);paint();
          if(item.id)uploadCall({kind:'upload_remove',upload_id:item.id}).catch(()=>{});
        });
        container.querySelectorAll('[data-upload-retry]').forEach(button=>button.onclick=()=>upload(d.files[Number(button.dataset.uploadRetry)]));
      };
      const upload=async item=>{
        item.error=null;paint();
        try{
          const data=new Uint8Array(await readFile(item.file));
          const started=await uploadCall({kind:'upload_start',request_key:item.key,name:item.file.name,byte_size:data.length,sha256:await digest(data)});item.id=started.upload_id;
          let offset=started.offset;
          while(offset<data.length&&!item.cancelled){const result=await uploadCall({kind:'upload_chunk',upload_id:item.id,offset,data_base64:bytes64(data.subarray(offset,offset+24576))});if(result.offset<=offset)throw new Error(t('uploadFailed','Upload did not advance. Retry.'));offset=result.offset;}
          if(item.cancelled){await uploadCall({kind:'upload_remove',upload_id:item.id});return;}
          item.uploaded=true;
        }catch(e){item.error=e.message;}
        paint();
      };
      container.querySelector('.ticket-attach').onclick=()=>container.querySelector('input[type=file]').click();
      container.querySelector('input[type=file]').onchange=async e=>{
        const files=[...e.target.files];e.target.value='';
        if(d.files.length+files.length>16 || [...d.files.map(f=>f.file),...files].reduce((n,f)=>n+f.size,0)>32*1048576 || files.some(f=>f.size===0||f.size>16*1048576)) {error(t('fileLimits','Use up to 16 files, 16 MiB each and 32 MiB per message.'),container.querySelector('.ticket-error')||undefined);return;}
        for(const file of files){const item={file,key:uuid(),uploaded:false};d.files.push(item);paint();await upload(item);}
      };
      paint();
    }

    function dialog(title) {
      const el=document.createElement('dialog');el.className='ticket-dialog';
      el.innerHTML=`<header class="dialog-head"><h2>${esc(title)}</h2><button type="button" class="btn" aria-label="${esc(t('closeDialog','Close dialog'))}">${icon('x')}</button></header><div class="ticket-dialog-content"></div>`;
      document.body.append(el);dialogs.add(el);const focus=document.activeElement;
      el.querySelector('header button').onclick=()=>el.close();
      el.addEventListener('close',()=>{dialogs.delete(el);el.remove();focus?.focus();},{once:true});el.showModal();return el;
    }
    function editor(ticket=null) {
      const d=ticket?{body:ticket.body,files:[],key:uuid()}:drafts.get(`${target}:new`)||{title:'',body:'',files:[],key:uuid()};
      if(!ticket)drafts.set(`${target}:new`,d);
      const el=dialog(ticket?t('editRequest','Edit request'):t('new','New request'));
      el.querySelector('.ticket-dialog-content').innerHTML=`<form><label>${text('subject','Subject')}<input name="title" maxlength="240" required value="${esc(ticket?.title||d.title||'')}"></label><label>${text('description','Description')}<textarea name="body" rows="5" maxlength="16384" required>${esc(d.body)}</textarea></label><p class="ticket-error" role="alert" hidden></p>${!ticket?`<div class="ticket-pending-files"></div><button type="button" class="btn ticket-attach">${icon('plus')}${text('attach','Attach files')}</button><input type="file" multiple hidden aria-label="${esc(t('attach','Attach files'))}">`:''}<p class="muted">${text('publicNotice','Replies and attachments are public.')}</p><div class="ticket-dialog-actions"><button type="button" class="btn" data-cancel>${text('cancel','Cancel')}</button><button type="submit" class="btn btn-primary">${text('save','Save request')}</button></div></form>`;
      const form=el.querySelector('form');form.elements.title.focus();
      form.elements.title.oninput=e=>d.title=e.target.value;form.elements.body.oninput=e=>d.body=e.target.value;
      form.querySelector('[data-cancel]').onclick=()=>el.close();if(!ticket)bindFiles(form,d);
      form.onsubmit=async e=>{e.preventDefault();const button=form.querySelector('[type=submit]');button.disabled=true;
        try{
          if(d.files.some(f=>!f.uploaded))throw new Error(t('waitFiles','Finish uploading or remove failed files before sending.'));
          const result=await call(ticket?{kind:'edit',ticket_id:ticket.id,expected_revision:ticket.revision,title:form.elements.title.value,body:form.elements.body.value}:{kind:'create',request_key:d.key,title:form.elements.title.value,body:form.elements.body.value,attachments:d.files.map(f=>f.id)});
          if(!ticket)drafts.delete(`${target}:new`);await loadList();await open(result.ticket.id);el.close();
        }catch(e){error(e.message,form.querySelector('.ticket-error'));}finally{button.disabled=false;}
      };
    }
    function configure() {
      const el=dialog(t('upstreamSettings','Upstream server'));
      el.querySelector('.ticket-dialog-content').innerHTML=`<form><label>${text('address','Server address')}<input name="upstream" value="${esc(settings.upstream)}" ${settings.can_configure?'':'readonly'}></label><p>${text('settingsHelp','New requests go to this server. Existing tickets stay with their owner. Use local to receive requests on this server.')}</p><p class="ticket-error" role="alert" hidden></p>${settings.previous_upstreams.length?`<label>${text('previous','Read requests at another saved upstream')}<select name="previous"><option value="">${t('chooseServer','Choose a server')}</option>${settings.previous_upstreams.map(value=>`<option>${esc(value)}</option>`).join('')}</select></label>`:''}<div class="ticket-dialog-actions"><button class="btn" type="button" data-cancel>${text('cancel','Cancel')}</button>${settings.can_configure?`<button class="btn btn-primary" type="submit">${text('saveSettings','Save setting')}</button>`:''}</div></form>`;
      const form=el.querySelector('form');form.querySelector('[data-cancel]').onclick=()=>el.close();
      if(form.elements.previous)form.elements.previous.onchange=()=>run(form.elements.previous,async()=>{if(!form.elements.previous.value)return;target=form.elements.previous.value;view='submitted';selected=null;el.close();shell();await loadList();});
      form.onsubmit=async e=>{e.preventDefault();try{settings=await api('ticket.configure',{upstream:form.elements.upstream.value,expected_revision:settings.revision});target=view==='received'?'local':settings.upstream;selected=null;el.close();shell();await loadList();}catch(e){error(e.message,form.querySelector('.ticket-error'));}};
    }
    async function readAttachment(ticketId,commentId,fileId) {
      const ownerTarget=target;let offset=0,file;const pieces=[];
      let total=0;
      do{
        const chunk=await api('ticket.request',{target:ownerTarget,action:{kind:'file',ticket_id:ticketId,comment_id:commentId,file_id:fileId,offset}});
        if(chunk.offset!==offset || chunk.attachment.id!==fileId || chunk.attachment.byte_size>16*1048576 || (file&&file.sha256!==chunk.attachment.sha256))throw new Error(t('fileChanged','The file could not be verified. Reload the ticket.'));
        file=chunk.attachment;const bytes=from64(chunk.data_base64);total+=bytes.length;
        if(total>file.byte_size || (chunk.next_offset!=null&&(chunk.next_offset!==total||chunk.next_offset<=offset)))throw new Error(t('fileChanged','The file could not be verified. Reload the ticket.'));
        pieces.push(bytes);offset=chunk.next_offset;
      }while(offset!=null&&active);
      if(!active)throw new Error('The view was closed.');
      const blob=new Blob(pieces,{type:file.content_type});const bytes=await blob.arrayBuffer();
      if(await digest(bytes)!==file.sha256||blob.size!==file.byte_size)throw new Error(t('fileChanged','The file could not be verified. Reload the ticket.'));
      return {blob,bytes,file};
    }
    async function preview(fileId,commentId) {
      const el=dialog(t('attachment','Attachment'));const body=el.querySelector('.ticket-dialog-content');body.textContent=t('loadingFile','Loading file…');
      try {
        const {blob,bytes,file}=await readAttachment(selected,commentId,fileId);
        if(!el.isConnected)return;
        const url=URL.createObjectURL(blob);urls.add(url);el.addEventListener('close',()=>{URL.revokeObjectURL(url);urls.delete(url);},{once:true});
        el.querySelector('header > h2').textContent=file.name;body.replaceChildren();
        const download=document.createElement('a');download.className='btn';download.href=url;download.download=file.name;download.textContent=t('download','Download original');body.append(download);
        if(file.content_type.startsWith('image/')){const image=document.createElement('img');image.src=url;image.alt=file.name;image.className='ticket-preview-image';body.append(image);}
        else if(file.content_type==='application/pdf'){
          const pdfjs=await import('/vendor/pdfjs/pdf.min.mjs');
          pdfjs.GlobalWorkerOptions.workerSrc='/vendor/pdfjs/pdf.worker.min.mjs';
          const task=pdfjs.getDocument({data:new Uint8Array(bytes),cMapUrl:'/vendor/pdfjs/cmaps/',cMapPacked:true,standardFontDataUrl:'/vendor/pdfjs/standard_fonts/',wasmUrl:'/vendor/pdfjs/wasm/',enableXfa:false});
          el.addEventListener('close',()=>{task.destroy();},{once:true});
          const pdf=await task.promise;
          const controls=document.createElement('div');controls.className='ticket-pdf-controls';
          const previous=document.createElement('button'),next=document.createElement('button'),position=document.createElement('span');
          previous.className=next.className='btn';previous.textContent=t('previousPage','Previous page');next.textContent=t('nextPage','Next page');controls.append(previous,position,next);body.append(controls);
          const canvas=document.createElement('canvas');canvas.className='ticket-pdf-page';canvas.setAttribute('role','img');canvas.setAttribute('aria-label',file.name);body.append(canvas);
          const accessible=document.createElement('details');accessible.className='ticket-pdf-text';const summary=document.createElement('summary');summary.textContent=t('documentText','Document text');const pre=document.createElement('pre');accessible.append(summary,pre);body.append(accessible);
          let number=1;
          const show=async()=>{
            previous.disabled=next.disabled=true;
            try{const page=await pdf.getPage(number);const original=page.getViewport({scale:1});const scale=Math.min(2,Math.max(1,body.clientWidth)/original.width);const viewport=page.getViewport({scale});canvas.width=Math.ceil(viewport.width);canvas.height=Math.ceil(viewport.height);await page.render({canvasContext:canvas.getContext('2d'),viewport,annotationMode:pdfjs.AnnotationMode.DISABLE}).promise;const content=await page.getTextContent();pre.textContent=content.items.map(item=>item.str||'').join(' ');position.textContent=`${number} / ${pdf.numPages}`;canvas.dataset.page=String(number);}
            finally{previous.disabled=number===1;next.disabled=number===pdf.numPages;}
          };
          previous.onclick=()=>{number--;show().catch(e=>error(e.message));};next.onclick=()=>{number++;show().catch(e=>error(e.message));};await show();
        }
        else if(file.content_type==='text/plain'){const pre=document.createElement('pre');pre.textContent=await blob.text();body.append(pre);}
        else if(/\.(docx|odt)$/i.test(file.name)){const pre=document.createElement('pre');pre.textContent=await officeText(bytes,/\.odt$/i.test(file.name)?'content.xml':'word/document.xml');body.append(pre);}
        else {const p=document.createElement('p');p.textContent=t('downloadToView','Download this format to view it in its application.');body.append(p);}
      }catch(e){body.textContent=e.message;}
    }
    try {
      settings=publicOnly?{upstream:location.origin}:await api('ticket.settings',{});
      const query=new URLSearchParams((publicOnly?location.search:location.hash.split('?')[1])||'');
      view=publicOnly?'received':query.get('view')==='received'?'received':'submitted';
      target=publicOnly?'local':query.get('target')||(view==='received'?'local':settings.upstream);
      shell();await loadList();if(query.get('ticket'))await open(query.get('ticket'));
    }catch(e){if(!root.querySelector('.ticket-error'))root.innerHTML='<h1>Feature requests</h1><p class="ticket-error" role="alert"></p>';error(e.message);}
    return abort;
  }

  // Extract the main text of bounded ZIP-based office documents locally. Never
  // load their external relationships, scripts, remote images or HTML content.
  async function officeText(buffer, wanted) {
    const view=new DataView(buffer), bytes=new Uint8Array(buffer);let end=-1;
    for(let i=bytes.length-22;i>=Math.max(0,bytes.length-65557);i--)if(view.getUint32(i,true)===0x06054b50){end=i;break;}
    if(end<0)throw new Error('This document could not be previewed. Download the original.');
    let at=view.getUint32(end+16,true);const count=view.getUint16(end+10,true);
    if(count>4096)throw new Error('Document contains too many entries to preview.');
    for(let i=0;i<count;i++){
      if(at+46>bytes.length||view.getUint32(at,true)!==0x02014b50)break;
      const method=view.getUint16(at+10,true),packed=view.getUint32(at+20,true),size=view.getUint32(at+24,true),nameLength=view.getUint16(at+28,true),extra=view.getUint16(at+30,true),comment=view.getUint16(at+32,true),local=view.getUint32(at+42,true);
      const name=new TextDecoder().decode(bytes.subarray(at+46,at+46+nameLength));at+=46+nameLength+extra+comment;
      if(name!==wanted)continue;
      if(size>4*1048576||local+30>bytes.length||view.getUint32(local,true)!==0x04034b50)throw new Error('Document preview exceeds the supported size.');
      const start=local+30+view.getUint16(local+26,true)+view.getUint16(local+28,true);
      if(start+packed>bytes.length)throw new Error('Document is incomplete.');
      let data=bytes.subarray(start,start+packed);
      if(method===8){const reader=new Blob([data]).stream().pipeThrough(new DecompressionStream('deflate-raw')).getReader();const parts=[];let total=0;try{for(;;){const part=await reader.read();if(part.done)break;total+=part.value.length;if(total>4*1048576)throw new Error('Document preview is too large.');parts.push(part.value);}}finally{await reader.cancel();}data=new Uint8Array(await new Blob(parts).arrayBuffer());}
      else if(method!==0)throw new Error('This document compression is unsupported.');
      const source=new TextDecoder().decode(data);
      if(/<!DOCTYPE|<!ENTITY/i.test(source))throw new Error('Document declarations are not supported in preview.');
      const xml=new DOMParser().parseFromString(source,'application/xml');
      if(xml.querySelector('parsererror'))throw new Error('Document text could not be read.');
      return [...xml.getElementsByTagNameNS('*','p')].map(p=>p.textContent).join('\n\n');
    }
    throw new Error('Document text was not found. Download the original.');
  }
  return {mount};
})();
