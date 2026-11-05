/*
===========================================================================
Copyright (C) 1999-2005 Id Software, Inc.

This file is part of Quake III Arena source code.

Quake III Arena source code is free software; you can redistribute it
and/or modify it under the terms of the GNU General Public License as
published by the Free Software Foundation; either version 2 of the License,
or (at your option) any later version.

Quake III Arena source code is distributed in the hope that it will be
useful, but WITHOUT ANY WARRANTY; without even the implied warranty of
MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
GNU General Public License for more details.

You should have received a copy of the GNU General Public License
along with Foobar; if not, write to the Free Software
Foundation, Inc., 51 Franklin St, Fifth Floor, Boston, MA  02110-1301  USA
===========================================================================
*/
#include <signal.h>
#include <stdio.h>
#include <unistd.h>

#include "../game/q_shared.h"
#include "../qcommon/qcommon.h"
#ifndef DEDICATED
#include "../renderer/tr_local.h"
#endif

static qboolean signalcaught = qfalse;;

void Sys_Exit(int); // bk010104 - abstraction

static void signal_handler(int sig) // bk010104 - replace this... (NOTE TTimo huh?)
{
  char msg[48];
  int len;

  if (signalcaught)
    _exit(1);

  signalcaught = qtrue;

  len = snprintf(msg, sizeof(msg), "Received signal %d, exiting...\n", sig);
  if (len > 0)
    write(2, msg, (size_t)len);

  // Tearing down X and GL from a signal handler takes locks the interrupted
  // thread may still hold, so the handler can deadlock and leave the client
  // (and its X grab) alive, which is how a crash freezes the whole session.
  // Only the terminating signals get the full shutdown; a fatal fault just
  // leaves, and the X server drops the connection with the process.
  switch (sig)
  {
    case SIGSEGV:
    case SIGBUS:
    case SIGILL:
    case SIGFPE:
    case SIGTRAP:
    case SIGABRT:
      _exit(1);
    default:
      break;
  }

#ifndef DEDICATED
  GLimp_Shutdown(); // bk010104 - shouldn't this be CL_Shutdown
#endif
  Sys_Exit(0); // bk010104 - abstraction NOTE TTimo send a 0 to avoid DOUBLE SIGNAL FAULT
}

void InitSig(void)
{
  signal(SIGHUP, signal_handler);
  signal(SIGQUIT, signal_handler);
  signal(SIGILL, signal_handler);
  signal(SIGTRAP, signal_handler);
  signal(SIGIOT, signal_handler);
  signal(SIGBUS, signal_handler);
  signal(SIGFPE, signal_handler);
  signal(SIGSEGV, signal_handler);
  signal(SIGTERM, signal_handler);
}
