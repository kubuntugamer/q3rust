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
// sdl_snd.c -- SDL2 audio backend
//
// The engine paints into a ring buffer (dma.buffer) whenever it likes and
// asks SNDDMA_GetDMAPos() how far the device has played. SDL owns the
// playback side: its audio callback copies out of the ring, so the play
// head advances with real device consumption and the engine's mix-ahead
// logic keeps working unchanged.
//
// Different from the OSS backend this replaces: no /dev/dsp, no mmap of a
// kernel buffer, no per-device ioctl probing, and no glibc memset bug
// workaround is needed because we own the buffer.

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include <SDL2/SDL.h>

#include "../game/q_shared.h"
#include "../client/snd_local.h"

// Ring size in interleaved samples. Must stay a power of two: the mixer's
// stereo fast path masks the write cursor against (dma.samples>>1)-1.
#define SDL_DMA_SAMPLES		16384

static qboolean			snd_inited = qfalse;
static SDL_AudioDeviceID	snd_device_id = 0;
static volatile int		snd_pos = 0;		// play head, interleaved samples

cvar_t *sndbits;
cvar_t *sndspeed;
cvar_t *sndchannels;
cvar_t *snddevice;

static cvar_t *s_sdlBuffer;
static char sndUsedDevice[256];

/*
==============
Snd_Memset

q_shared.h declares this for __linux__; snd_dma.c uses it to clear the ring.
With an SDL-owned buffer there is no mmap'd kernel page to work around, so
this is a plain memset.
==============
*/
void Snd_Memset( void *dest, const int val, const size_t count )
{
	Com_Memset( dest, val, count );
}

/*
==============
SND_Silence
==============
*/
static void SND_Silence( Uint8 *stream, int len )
{
	memset( stream, ( dma.samplebits == 8 ) ? 0x80 : 0x00, len );
}

/*
==============
SND_PaintAudio

SDL audio callback. Runs on SDL's audio thread with the device locked, so
it cannot race SNDDMA_BeginPainting()/SNDDMA_Submit() on the main thread.
==============
*/
static void SND_PaintAudio( void *userdata, Uint8 *stream, int len )
{
	int		bps;
	int		pos, remaining;

	(void)userdata;

	if ( !snd_inited || !dma.buffer )
	{
		SND_Silence( stream, len );
		return;
	}

	bps = dma.samplebits / 8;
	if ( bps <= 0 || dma.samples <= 0 )
	{
		SND_Silence( stream, len );
		return;
	}

	pos = snd_pos;
	remaining = len / bps;

	while ( remaining > 0 )
	{
		int chunk = dma.samples - pos;

		if ( chunk > remaining )
		{
			chunk = remaining;
		}
		if ( chunk <= 0 )
		{
			pos = 0;
			continue;
		}

		memcpy( stream, dma.buffer + (size_t)pos * bps, (size_t)chunk * bps );
		stream += (size_t)chunk * bps;
		remaining -= chunk;
		pos += chunk;

		if ( pos >= dma.samples )
		{
			pos = 0;
		}
	}

	snd_pos = pos;
}

/*
==============
SND_PrintDevices_f
==============
*/
static void SND_PrintDevices_f( void )
{
	int		i, count;

	count = SDL_GetNumAudioDevices( 0 );
	Com_Printf( "SDL audio driver: %s\n", SDL_GetCurrentAudioDriver() );
	Com_Printf( "%d playback device(s):\n", count );
	for ( i = 0; i < count; i++ )
	{
		Com_Printf( "  %2d: %s\n", i, SDL_GetAudioDeviceName( i, 0 ) );
	}
}

/*
==============
SND_ResolveDevice

snddevice selects the SDL playback device by exact name, by index, or is
empty (the system default). The old OSS default "/dev/dsp" is accepted and
ignored so existing configs keep working.
==============
*/
static const char *SND_ResolveDevice( void )
{
	const char	*want;
	int			i, count, index;

	want = snddevice ? snddevice->string : "";
	if ( !want[0] || !Q_stricmp( want, "default" ) )
	{
		Q_strncpyz( sndUsedDevice, "default", sizeof( sndUsedDevice ) );
		return NULL;
	}

	if ( want[0] == '/' )
	{
		Com_Printf( "SDL audio: snddevice \"%s\" is an OSS path, using the default device\n", want );
		Com_Printf( "SDL audio: use s_devices to list names, or set snddevice to a name or index\n" );
		Q_strncpyz( sndUsedDevice, "default", sizeof( sndUsedDevice ) );
		return NULL;
	}

	count = SDL_GetNumAudioDevices( 0 );

	if ( want[0] >= '0' && want[0] <= '9' )
	{
		index = atoi( want );
		if ( index >= 0 && index < count )
		{
			const char *name = SDL_GetAudioDeviceName( index, 0 );
			if ( name )
			{
				Q_strncpyz( sndUsedDevice, name, sizeof( sndUsedDevice ) );
				return sndUsedDevice;
			}
		}
		Com_Printf( "SDL audio: no device at index %d, using the default device\n", index );
		Q_strncpyz( sndUsedDevice, "default", sizeof( sndUsedDevice ) );
		return NULL;
	}

	for ( i = 0; i < count; i++ )
	{
		const char *name = SDL_GetAudioDeviceName( i, 0 );

		if ( name && !Q_stricmp( name, want ) )
		{
			Q_strncpyz( sndUsedDevice, name, sizeof( sndUsedDevice ) );
			return sndUsedDevice;
		}
	}

	Com_Printf( "SDL audio: no device named \"%s\", using the default device\n", want );
	Com_Printf( "SDL audio: use s_devices to list available devices\n" );
	Q_strncpyz( sndUsedDevice, "default", sizeof( sndUsedDevice ) );
	return NULL;
}

/*
==============
SNDDMA_Init
==============
*/
qboolean SNDDMA_Init( void )
{
	SDL_AudioSpec	desired, obtained;
	const char		*devName;
	int				frames, bufBytes;

	if ( snd_inited )
	{
		return qtrue;
	}

	sndbits = Cvar_Get( "sndbits", "16", CVAR_ARCHIVE );
	sndspeed = Cvar_Get( "sndspeed", "0", CVAR_ARCHIVE );
	sndchannels = Cvar_Get( "sndchannels", "2", CVAR_ARCHIVE );
	snddevice = Cvar_Get( "snddevice", "", CVAR_ARCHIVE );
	s_sdlBuffer = Cvar_Get( "s_sdlBuffer", "1024", CVAR_ARCHIVE );

	Cmd_AddCommand( "s_devices", SND_PrintDevices_f );

	if ( SDL_InitSubSystem( SDL_INIT_AUDIO ) != 0 )
	{
		Com_Printf( "SDL audio: initialization failed: %s\n", SDL_GetError() );
		return qfalse;
	}

	if ( !Q_stricmp( snddevice->string, "list" ) )
	{
		SND_PrintDevices_f();
		SDL_QuitSubSystem( SDL_INIT_AUDIO );
		return qfalse;
	}

	dma.samplebits = (int)sndbits->value;
	if ( dma.samplebits != 8 && dma.samplebits != 16 )
	{
		dma.samplebits = 16;
	}

	dma.channels = (int)sndchannels->value;
	if ( dma.channels < 1 || dma.channels > 2 )
	{
		dma.channels = 2;
	}

	dma.speed = (int)sndspeed->value;
	if ( dma.speed <= 0 )
	{
		dma.speed = 22050;
	}

	frames = (int)s_sdlBuffer->value;
	if ( frames < 128 )
	{
		frames = 128;
	}
	if ( frames > 8192 )
	{
		frames = 8192;
	}

	devName = SND_ResolveDevice();

	memset( &desired, 0, sizeof( desired ) );
	desired.freq = dma.speed;
	desired.format = ( dma.samplebits == 8 ) ? AUDIO_U8 : AUDIO_S16SYS;
	desired.channels = (Uint8)dma.channels;
	desired.samples = (Uint16)frames;
	desired.callback = SND_PaintAudio;

	snd_device_id = SDL_OpenAudioDevice( devName, 0, &desired, &obtained,
		SDL_AUDIO_ALLOW_FREQUENCY_CHANGE );
	if ( !snd_device_id )
	{
		Com_Printf( "SDL audio: could not open \"%s\": %s\n",
			sndUsedDevice, SDL_GetError() );
		SDL_QuitSubSystem( SDL_INIT_AUDIO );
		return qfalse;
	}

	// Adopt what the device actually gave us.
	dma.speed = obtained.freq;
	dma.channels = obtained.channels;
	dma.samplebits = ( obtained.format == AUDIO_U8 ) ? 8 : 16;
	dma.samples = SDL_DMA_SAMPLES;
	dma.submission_chunk = 1;

	bufBytes = dma.samples * ( dma.samplebits / 8 );
	dma.buffer = (byte *)malloc( bufBytes );
	if ( !dma.buffer )
	{
		Com_Printf( "SDL audio: could not allocate %d byte ring buffer\n", bufBytes );
		SDL_CloseAudioDevice( snd_device_id );
		snd_device_id = 0;
		SDL_QuitSubSystem( SDL_INIT_AUDIO );
		return qfalse;
	}

	Com_Memset( dma.buffer, ( dma.samplebits == 8 ) ? 0x80 : 0x00, bufBytes );

	snd_pos = 0;
	snd_inited = qtrue;

	SDL_PauseAudioDevice( snd_device_id, 0 );

	Com_Printf( "SDL audio: %s (%s), %d Hz, %d channel(s), %d bit, %d frames/callback\n",
		sndUsedDevice, SDL_GetCurrentAudioDriver(), dma.speed, dma.channels,
		dma.samplebits, obtained.samples );

	return qtrue;
}

/*
==============
SNDDMA_GetDMAPos
==============
*/
int SNDDMA_GetDMAPos( void )
{
	if ( !snd_inited )
	{
		return 0;
	}
	return snd_pos;
}

/*
==============
SNDDMA_Shutdown
==============
*/
void SNDDMA_Shutdown( void )
{
	if ( !snd_inited && !snd_device_id && !dma.buffer )
	{
		return;
	}

	if ( snd_device_id )
	{
		SDL_PauseAudioDevice( snd_device_id, 1 );
		SDL_CloseAudioDevice( snd_device_id );
		snd_device_id = 0;
	}

	if ( dma.buffer )
	{
		free( dma.buffer );
		dma.buffer = NULL;
	}

	dma.samples = 0;
	dma.channels = 0;
	dma.samplebits = 0;
	dma.submission_chunk = 0;
	snd_pos = 0;
	snd_inited = qfalse;

	Cmd_RemoveCommand( "s_devices" );

	SDL_QuitSubSystem( SDL_INIT_AUDIO );
}

/*
==============
SNDDMA_BeginPainting

Blocks the audio callback so the engine can write the ring without the play
head moving underneath it.
==============
*/
void SNDDMA_BeginPainting( void )
{
	if ( snd_device_id )
	{
		SDL_LockAudioDevice( snd_device_id );
	}
}

/*
==============
SNDDMA_Submit

Send sound to device if buffer isn't really the dma buffer.
==============
*/
void SNDDMA_Submit( void )
{
	if ( snd_device_id )
	{
		SDL_UnlockAudioDevice( snd_device_id );
	}
}

/*
==============
SNDDMA_Activate
==============
*/
void SNDDMA_Activate( void )
{
}
