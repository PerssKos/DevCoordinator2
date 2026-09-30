'use strict';
window.DevCoordinatorTickets.mount(document.getElementById('tickets'),{publicOnly:true,api:async(_operation,{action})=>{
  const response=await fetch('/.well-known/devcoordinator2/tickets',{method:'POST',headers:{'content-type':'application/json'},body:JSON.stringify({action,credential:null,server_label:null})});
  const result=await response.json();if(!result.ok)throw new Error(result.error?.message||'Ticket server unavailable');return result.data;
}});
